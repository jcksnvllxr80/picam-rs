#include "camera_ffi.h"

#include <libcamera/libcamera.h>
#include <libcamera/base/object.h>

#include <sys/mman.h>
#include <atomic>
#include <condition_variable>
#include <cstring>
#include <map>
#include <memory>
#include <mutex>
#include <vector>

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

struct PicamHandle : public Object {
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

    // pending controls (written by setters, consumed on next queued request)
    std::mutex   ctrl_mutex;
    ControlList  pending;
    bool         ctrl_dirty = false;

    std::atomic<bool> recording { false };
    std::atomic<bool> streaming { false };

    // sensor pixel array size for ScalerCrop ROI computation
    Size sensor_size;

    // ROI (applied only when != full-frame)
    float roi_x = 0.f, roi_y = 0.f, roi_w = 1.f, roi_h = 1.f;
    bool  roi_dirty = false;

    PicamHandle() : pending(controls::controls) {}

    bool start_streams();
    void stop_streams();
    void on_request_complete(Request *req);
};

// ── Stream start/stop ──────────────────────────────────────────────────────────

bool PicamHandle::start_streams()
{
    if (streaming.load()) return true;

    // Re-apply configuration (needed after stop)
    if (config) {
        if (camera->configure(config.get()) < 0) return false;
    }

    // Queue all pre-allocated requests
    camera->start();
    for (auto &req : requests) {
        req->reuse(Request::ReuseBuffers);
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

    // Apply any pending controls on the recycled request
    {
        std::lock_guard<std::mutex> lk(ctrl_mutex);
        if (ctrl_dirty) {
            req->controls().merge(pending);
            pending = ControlList(controls::controls);
            ctrl_dirty = false;
        }
        if (roi_dirty) {
            // Convert normalised ROI to sensor pixel rectangle
            Rectangle sensor(sensor_size);
            int32_t sx = static_cast<int32_t>(roi_x * sensor_size.width);
            int32_t sy = static_cast<int32_t>(roi_y * sensor_size.height);
            int32_t sw = static_cast<int32_t>(roi_w * sensor_size.width);
            int32_t sh = static_cast<int32_t>(roi_h * sensor_size.height);
            req->controls().set(controls::ScalerCrop, Rectangle(sx, sy, sw, sh));
            roi_dirty = false;
        }
    }

    req->reuse(Request::ReuseBuffers);
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

    // Connect completion signal
    cam->camera->requestCompleted.connect(cam.get(), &PicamHandle::on_request_complete);

    // Start streaming
    cam->camera->start();
    for (auto &req : cam->requests)
        cam->camera->queueRequest(req.get());
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

void picam_pause(PicamHandle *cam)
{
    if (cam) cam->stop_streams();
}

void picam_resume(PicamHandle *cam)
{
    if (cam) cam->start_streams();
}

// ── Settings ──────────────────────────────────────────────────────────────────

void picam_set_gain(PicamHandle *cam, float gain)
{
    if (!cam) return;
    std::lock_guard<std::mutex> lk(cam->ctrl_mutex);
    if (gain <= 0.f) {
        cam->pending.set(controls::AnalogueGainMode,
                         static_cast<int32_t>(controls::AnalogueGainModeAuto));
    } else {
        cam->pending.set(controls::AnalogueGainMode,
                         static_cast<int32_t>(controls::AnalogueGainModeManual));
        cam->pending.set(controls::AnalogueGain, gain);
    }
    cam->ctrl_dirty = true;
}

void picam_set_shutter(PicamHandle *cam, int32_t us)
{
    if (!cam) return;
    std::lock_guard<std::mutex> lk(cam->ctrl_mutex);
    if (us <= 0) {
        cam->pending.set(controls::ExposureTimeMode,
                         static_cast<int32_t>(controls::ExposureTimeModeAuto));
    } else {
        cam->pending.set(controls::ExposureTimeMode,
                         static_cast<int32_t>(controls::ExposureTimeModeManual));
        cam->pending.set(controls::ExposureTime, us);
    }
    cam->ctrl_dirty = true;
}

void picam_set_awb(PicamHandle *cam, int32_t mode_idx)
{
    if (!cam) return;
    std::lock_guard<std::mutex> lk(cam->ctrl_mutex);
    cam->pending.set(controls::AwbMode, mode_idx);
    cam->ctrl_dirty = true;
}

void picam_set_ev(PicamHandle *cam, float ev)
{
    if (!cam) return;
    std::lock_guard<std::mutex> lk(cam->ctrl_mutex);
    cam->pending.set(controls::ExposureValue, ev);
    cam->ctrl_dirty = true;
}

void picam_set_contrast(PicamHandle *cam, float v)
{
    if (!cam) return;
    std::lock_guard<std::mutex> lk(cam->ctrl_mutex);
    cam->pending.set(controls::Contrast, v);
    cam->ctrl_dirty = true;
}

void picam_set_saturation(PicamHandle *cam, float v)
{
    if (!cam) return;
    std::lock_guard<std::mutex> lk(cam->ctrl_mutex);
    cam->pending.set(controls::Saturation, v);
    cam->ctrl_dirty = true;
}

void picam_set_sharpness(PicamHandle *cam, float v)
{
    if (!cam) return;
    std::lock_guard<std::mutex> lk(cam->ctrl_mutex);
    cam->pending.set(controls::Sharpness, v);
    cam->ctrl_dirty = true;
}

void picam_set_brightness(PicamHandle *cam, float v)
{
    if (!cam) return;
    std::lock_guard<std::mutex> lk(cam->ctrl_mutex);
    cam->pending.set(controls::Brightness, v);
    cam->ctrl_dirty = true;
}

void picam_set_roi(PicamHandle *cam, float x, float y, float w, float h)
{
    if (!cam) return;
    std::lock_guard<std::mutex> lk(cam->ctrl_mutex);
    cam->roi_x = x; cam->roi_y = y;
    cam->roi_w = w; cam->roi_h = h;
    cam->roi_dirty = true;
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
