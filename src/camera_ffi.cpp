#include "camera_ffi.h"

#include <libcamera/libcamera.h>

#include <sys/mman.h>
#include <atomic>
#include <condition_variable>
#include <cstdio>
#include <cstring>
#include <map>
#include <memory>
#include <mutex>
#include <vector>

#define PLOG(...) do { fprintf(stderr, "[picam-ffi] " __VA_ARGS__); fputc('\n', stderr); fflush(stderr); } while (0)

using namespace libcamera;

// ── Buffer mmap cache ─────────────────────────────────────────────────────────

struct MappedBuf {
    void  *ptr;
    size_t size;
};

static MappedBuf map_fb(const FrameBuffer *fb)
{
    size_t total = 0;
    for (const auto &plane : fb->planes())
        total = std::max(total, static_cast<size_t>(plane.offset + plane.length));

    int fd = fb->planes()[0].fd.get();
    void *p = ::mmap(nullptr, total, PROT_READ, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) return { nullptr, 0 };
    return { p, total };
}

// ── Internal camera context ────────────────────────────────────────────────────

// NOT a libcamera::Object — that would queue completion callbacks to this
// object's thread, which has no event dispatcher. Plain struct means
// requestCompleted fires directly on libcamera's completion thread.
struct PicamHandle {
    // libcamera objects
    std::shared_ptr<CameraManager>       manager;
    std::shared_ptr<Camera>              camera;
    std::unique_ptr<FrameBufferAllocator> allocator;
    std::unique_ptr<CameraConfiguration> config;

    Stream *preview_stream = nullptr;
    Stream *record_stream  = nullptr;

    std::vector<std::unique_ptr<Request>> requests;
    std::map<const FrameBuffer *, MappedBuf> buf_map;

    // callbacks
    PicamFrameCb preview_cb;
    PicamFrameCb record_cb;
    void        *user_ctx;

    uint32_t preview_w, preview_h;
    uint32_t record_w,  record_h;

    // Per-control state. picam_set_X writes these atomically; on every
    // request_complete we build a fresh ControlList from them and merge into
    // the (just-reused) request before re-queueing. We CANNOT just set
    // controls once because:
    //   1) Request::reuse(ReuseBuffers) wipes controls — they must be set
    //      AFTER the reuse, not before
    //   2) libcamera doesn't auto-propagate per-frame controls (AnalogueGain,
    //      ExposureTime, Contrast, etc.) to subsequent requests — they need
    //      to be set on every frame to take effect
    std::atomic<bool>    manual_gain    { false };
    std::atomic<float>   gain_value     { 0.f };
    std::atomic<bool>    manual_shutter { false };
    std::atomic<int32_t> shutter_value  { 0 };
    std::atomic<int32_t> awb_mode       { 0 };
    std::atomic<float>   ev_value       { 0.f };
    std::atomic<float>   contrast       { 1.f };
    std::atomic<float>   saturation     { 1.f };
    std::atomic<float>   sharpness      { 1.f };
    std::atomic<float>   brightness     { 0.f };
    std::atomic<bool>    roi_enabled    { false };
    std::atomic<float>   roi_x          { 0.f };
    std::atomic<float>   roi_y          { 0.f };
    std::atomic<float>   roi_w          { 1.f };
    std::atomic<float>   roi_h          { 1.f };

    std::atomic<bool> recording { false };
    std::atomic<bool> streaming { false };

    // sensor pixel array size for ScalerCrop ROI computation
    Size sensor_size;

    PicamHandle() = default;

    bool start_streams();
    void stop_streams();
    void on_request_complete(Request *req);
    void apply_controls(Request *req);
};

void PicamHandle::apply_controls(Request *req)
{
    auto &c = req->controls();
    bool any_manual = manual_gain.load() || manual_shutter.load();
    c.set(controls::AeEnable, !any_manual);
    if (manual_gain.load()) {
        c.set(controls::AnalogueGain, gain_value.load());
    }
    if (manual_shutter.load()) {
        // Cap the preview/record stream shutter at 1 second. Anything longer
        // (deep-sky exposures) freezes the live view for the full exposure
        // duration. rpicam-still runs as a separate subprocess for actual
        // photo capture and uses the unclamped user value via --shutter,
        // so long exposures still happen — just not in the live preview.
        int32_t us = shutter_value.load();
        if (us > 1'000'000) us = 1'000'000;
        c.set(controls::ExposureTime, us);
    }
    c.set(controls::AwbMode, awb_mode.load());
    float ev = ev_value.load();
    if (std::abs(ev) > 0.01f) {
        c.set(controls::ExposureValue, ev);
    }
    c.set(controls::Contrast,   contrast.load());
    c.set(controls::Saturation, saturation.load());
    c.set(controls::Sharpness,  sharpness.load());
    c.set(controls::Brightness, brightness.load());
    if (roi_enabled.load()) {
        int32_t sx = static_cast<int32_t>(roi_x.load() * sensor_size.width);
        int32_t sy = static_cast<int32_t>(roi_y.load() * sensor_size.height);
        int32_t sw = static_cast<int32_t>(roi_w.load() * sensor_size.width);
        int32_t sh = static_cast<int32_t>(roi_h.load() * sensor_size.height);
        c.set(controls::ScalerCrop, Rectangle(sx, sy, sw, sh));
    }
}

// ── Stream start/stop ──────────────────────────────────────────────────────────

bool PicamHandle::start_streams()
{
    if (streaming.load()) return true;

    // Re-apply configuration (needed after stop)
    if (config) {
        if (camera->configure(config.get()) < 0) return false;
    }

    // Queue all pre-allocated requests with current control state applied.
    camera->start();
    for (auto &req : requests) {
        req->reuse(Request::ReuseBuffers);
        apply_controls(req.get());
        camera->queueRequest(req.get());
    }
    streaming.store(true);
    return true;
}

void PicamHandle::stop_streams()
{
    if (!streaming.load()) return;
    streaming.store(false);
    camera->stop();
}

// ── Request completion ─────────────────────────────────────────────────────────

void PicamHandle::on_request_complete(Request *req)
{
    if (!streaming.load() || req->status() == Request::RequestCancelled) {
        return;
    }

    // Deliver completed frames first
    const auto &bufs = req->buffers();
    for (const auto &[stream, fb] : bufs) {
        auto it = buf_map.find(fb);
        if (it == buf_map.end()) continue;
        const MappedBuf &mb = it->second;
        if (!mb.ptr) continue;
        const uint8_t *data = static_cast<const uint8_t *>(mb.ptr);
        if (stream == preview_stream && preview_cb) {
            preview_cb(data, mb.size, preview_w, preview_h, user_ctx);
        } else if (stream == record_stream && record_cb && recording.load()) {
            record_cb(data, mb.size, record_w, record_h, user_ctx);
        }
    }

    // CRITICAL ORDER: reuse() resets the request (clears controls). We must
    // apply controls AFTER reuse, before re-queueing, or the IPA gets an
    // empty control list and falls back to full auto.
    req->reuse(Request::ReuseBuffers);
    apply_controls(req);

    if (streaming.load())
        camera->queueRequest(req);
}

// ── C API ──────────────────────────────────────────────────────────────────────

PicamHandle *picam_open(uint32_t preview_w, uint32_t preview_h,
                        uint32_t record_w,  uint32_t record_h,
                        PicamFrameCb preview_cb,
                        PicamFrameCb record_cb,
                        void *ctx)
{
    auto cam = std::make_unique<PicamHandle>();
    cam->preview_w  = preview_w;
    cam->preview_h  = preview_h;
    cam->record_w   = record_w;
    cam->record_h   = record_h;
    cam->preview_cb = preview_cb;
    cam->record_cb  = record_cb;
    cam->user_ctx   = ctx;

    // Camera manager
    cam->manager = std::make_shared<CameraManager>();
    if (cam->manager->start() < 0) return nullptr;

    auto cameras = cam->manager->cameras();
    if (cameras.empty()) return nullptr;

    cam->camera = cam->manager->get(cameras[0]->id());
    if (!cam->camera) return nullptr;
    if (cam->camera->acquire() < 0) return nullptr;

    // Configuration: lores (preview) + main (recording)
    cam->config = cam->camera->generateConfiguration(
        { StreamRole::Viewfinder, StreamRole::VideoRecording });
    if (!cam->config || cam->config->size() < 2) return nullptr;

    StreamConfiguration &preview_cfg = cam->config->at(0);
    StreamConfiguration &record_cfg  = cam->config->at(1);

    preview_cfg.pixelFormat = formats::NV12;
    preview_cfg.size        = { preview_w, preview_h };
    preview_cfg.bufferCount = 4;

    record_cfg.pixelFormat  = formats::NV12;
    record_cfg.size         = { record_w, record_h };
    record_cfg.bufferCount  = 4;

    cam->config->orientation = Orientation::Rotate180;

    if (cam->config->validate() == CameraConfiguration::Invalid) return nullptr;

    if (cam->camera->configure(cam->config.get()) < 0) return nullptr;

    cam->preview_stream = preview_cfg.stream();
    cam->record_stream  = record_cfg.stream();

    // Read sensor array size for ScalerCrop
    const ControlInfoMap &info = cam->camera->controls();
    auto it = info.find(controls::ScalerCrop.id());
    if (it != info.end()) {
        cam->sensor_size = it->second.max().get<Rectangle>().size();
    } else {
        cam->sensor_size = { 4056, 3040 }; // IMX477 fallback
    }

    // Allocate buffers
    cam->allocator = std::make_unique<FrameBufferAllocator>(cam->camera);
    for (auto &cfg : *cam->config) {
        if (cam->allocator->allocate(cfg.stream()) < 0) return nullptr;
    }

    // Build requests and mmap all buffers
    constexpr int N_BUFS = 4;
    const auto &preview_bufs = cam->allocator->buffers(cam->preview_stream);
    const auto &record_bufs  = cam->allocator->buffers(cam->record_stream);

    int n = std::min({ N_BUFS,
                       static_cast<int>(preview_bufs.size()),
                       static_cast<int>(record_bufs.size()) });

    for (int i = 0; i < n; i++) {
        auto req = cam->camera->createRequest();
        if (!req) return nullptr;

        FrameBuffer *pb = preview_bufs[i].get();
        FrameBuffer *rb = record_bufs[i].get();

        if (req->addBuffer(cam->preview_stream, pb) < 0) return nullptr;
        if (req->addBuffer(cam->record_stream,  rb) < 0) return nullptr;

        cam->buf_map[pb] = map_fb(pb);
        cam->buf_map[rb] = map_fb(rb);

        cam->requests.push_back(std::move(req));
    }

    PLOG("picam_open: configured, %d requests built, mmapped %d buffers", n, (int)cam->buf_map.size());

    // Connect completion signal
    cam->camera->requestCompleted.connect(cam.get(), &PicamHandle::on_request_complete);
    PLOG("picam_open: signal connected");

    // Start streaming
    int start_ret = cam->camera->start();
    PLOG("picam_open: camera->start() returned %d", start_ret);
    if (start_ret < 0) return nullptr;

    int queued = 0;
    for (auto &req : cam->requests) {
        cam->apply_controls(req.get());
        int q = cam->camera->queueRequest(req.get());
        if (q == 0) queued++;
        else PLOG("queueRequest failed: %d", q);
    }
    PLOG("picam_open: queued %d/%d requests, streaming=true", queued, (int)cam->requests.size());
    cam->streaming.store(true);

    return cam.release();
}

void picam_close(PicamHandle *cam)
{
    if (!cam) return;
    cam->stop_streams();

    for (auto &[fb, mb] : cam->buf_map)
        if (mb.ptr) ::munmap(mb.ptr, mb.size);

    cam->requests.clear();
    cam->allocator.reset();
    cam->camera->release();
    cam->camera.reset();
    cam->manager->stop();
    cam->manager.reset();
    delete cam;
}

// Pause fully tears down so rpicam-still can grab the camera. Stopping the
// streams alone leaves libcamera holding the pipeline handler. Resume
// rebuilds buffers + requests because rpicam-still may have reconfigured the
// camera while we weren't looking, invalidating the previous pointers.
void picam_pause(PicamHandle *cam)
{
    if (!cam) return;
    cam->stop_streams();
    for (auto &[fb, mb] : cam->buf_map)
        if (mb.ptr) ::munmap(mb.ptr, mb.size);
    cam->buf_map.clear();
    cam->requests.clear();
    cam->allocator.reset();
    cam->camera->release();
    PLOG("picam_pause: camera released");
}

void picam_resume(PicamHandle *cam)
{
    if (!cam) return;
    if (cam->camera->acquire() < 0) {
        PLOG("picam_resume: camera->acquire() failed");
        return;
    }
    if (cam->camera->configure(cam->config.get()) < 0) {
        PLOG("picam_resume: configure failed");
        return;
    }
    cam->allocator = std::make_unique<FrameBufferAllocator>(cam->camera);
    for (auto &cfg : *cam->config) {
        if (cam->allocator->allocate(cfg.stream()) < 0) {
            PLOG("picam_resume: allocate failed");
            return;
        }
    }
    constexpr int N_BUFS = 4;
    const auto &preview_bufs = cam->allocator->buffers(cam->preview_stream);
    const auto &record_bufs  = cam->allocator->buffers(cam->record_stream);
    int n = std::min({ N_BUFS,
                       static_cast<int>(preview_bufs.size()),
                       static_cast<int>(record_bufs.size()) });
    for (int i = 0; i < n; i++) {
        auto req = cam->camera->createRequest();
        if (!req) return;
        FrameBuffer *pb = preview_bufs[i].get();
        FrameBuffer *rb = record_bufs[i].get();
        if (req->addBuffer(cam->preview_stream, pb) < 0) return;
        if (req->addBuffer(cam->record_stream,  rb) < 0) return;
        cam->buf_map[pb] = map_fb(pb);
        cam->buf_map[rb] = map_fb(rb);
        cam->requests.push_back(std::move(req));
    }
    cam->camera->start();
    for (auto &req : cam->requests) {
        cam->apply_controls(req.get());
        cam->camera->queueRequest(req.get());
    }
    cam->streaming.store(true);
    PLOG("picam_resume: rebuilt %d requests, streaming", n);
}

// ── Settings ──────────────────────────────────────────────────────────────────

// All setters just update atomic state. apply_controls() reads them on
// every request_complete and re-applies the full control set to each
// reused request — this is the only way to get per-frame controls
// (gain, exposure, contrast, etc.) to actually persist.

void picam_set_gain(PicamHandle *cam, float gain)
{
    if (!cam) return;
    if (gain > 0.f) {
        cam->gain_value.store(gain);
        cam->manual_gain.store(true);
    } else {
        cam->manual_gain.store(false);
    }
}

void picam_set_shutter(PicamHandle *cam, int32_t us)
{
    if (!cam) return;
    if (us > 0) {
        cam->shutter_value.store(us);
        cam->manual_shutter.store(true);
    } else {
        cam->manual_shutter.store(false);
    }
}

void picam_set_awb       (PicamHandle *cam, int32_t mode_idx) { if (cam) cam->awb_mode.store(mode_idx); }
void picam_set_ev        (PicamHandle *cam, float ev)         { if (cam) cam->ev_value.store(ev); }
void picam_set_contrast  (PicamHandle *cam, float v)          { if (cam) cam->contrast.store(v); }
void picam_set_saturation(PicamHandle *cam, float v)          { if (cam) cam->saturation.store(v); }
void picam_set_sharpness (PicamHandle *cam, float v)          { if (cam) cam->sharpness.store(v); }
void picam_set_brightness(PicamHandle *cam, float v)          { if (cam) cam->brightness.store(v); }

void picam_set_roi(PicamHandle *cam, float x, float y, float w, float h)
{
    if (!cam) return;
    bool full_frame = (x <= 0.001f && y <= 0.001f && w >= 0.999f && h >= 0.999f);
    cam->roi_x.store(x);
    cam->roi_y.store(y);
    cam->roi_w.store(w);
    cam->roi_h.store(h);
    cam->roi_enabled.store(!full_frame);
}

// ── Recording ─────────────────────────────────────────────────────────────────

void picam_start_recording(PicamHandle *cam)
{
    if (cam) cam->recording.store(true);
}

void picam_stop_recording(PicamHandle *cam)
{
    if (cam) cam->recording.store(false);
}

int picam_is_recording(PicamHandle *cam)
{
    return cam && cam->recording.load() ? 1 : 0;
}
// rebuild 1778887526
// 1778887544831862274
