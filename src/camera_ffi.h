#pragma once
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct PicamHandle PicamHandle;

/*
 * Called on the libcamera completion thread for every decoded frame.
 *   data   : NV12 frame bytes (Y plane then interleaved UV plane)
 *   len    : total byte length of data
 *   width  : frame width in pixels
 *   height : frame height in pixels
 *   ctx    : user pointer passed to picam_open
 */
typedef void (*PicamFrameCb)(const uint8_t *data, size_t len,
                             uint32_t width, uint32_t height,
                             void *ctx);

/*
 * Open the camera with dual streams:
 *   preview : preview_w × preview_h NV12  (delivered to preview_cb every frame)
 *   record  : record_w  × record_h  NV12  (delivered to record_cb when recording)
 *
 * Returns NULL on failure.
 */
PicamHandle *picam_open(uint32_t preview_w, uint32_t preview_h,
                        uint32_t record_w,  uint32_t record_h,
                        PicamFrameCb preview_cb,
                        PicamFrameCb record_cb,
                        void *ctx);

void picam_close(PicamHandle *cam);

/* Temporarily stop streaming (releases camera resource for rpicam-still). */
void picam_pause(PicamHandle *cam);
void picam_resume(PicamHandle *cam);

/* Camera settings — applied on the next queued request. */
void picam_set_gain      (PicamHandle *cam, float gain);      /* 0 = auto */
void picam_set_shutter   (PicamHandle *cam, int32_t us);      /* 0 = auto */
void picam_set_awb       (PicamHandle *cam, int32_t mode_idx);
void picam_set_ev        (PicamHandle *cam, float ev);
void picam_set_contrast  (PicamHandle *cam, float v);
void picam_set_saturation(PicamHandle *cam, float v);
void picam_set_sharpness (PicamHandle *cam, float v);
void picam_set_brightness(PicamHandle *cam, float v);
void picam_set_roi       (PicamHandle *cam, float x, float y, float w, float h);

/* Toggle the record stream callback on/off. */
void picam_start_recording(PicamHandle *cam);
void picam_stop_recording (PicamHandle *cam);
int  picam_is_recording   (PicamHandle *cam);

#ifdef __cplusplus
}
#endif
