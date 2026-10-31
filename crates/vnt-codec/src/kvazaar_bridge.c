// Narrow ABI over the pinned Kvazaar headers; Rust never mirrors library structs.
#include "kvazaar.h"
#include <stdlib.h>
#include <string.h>

typedef struct {
  const kvz_api *api;
  kvz_encoder *encoder;
  int width, height;
  uint8_t *output;
  uint32_t capacity;
} vnt_hevc;

static void copy_plane(uint8_t *dst, int stride, int padded_height,
                       const uint8_t *src, int width, int height) {
  for (int y = 0; y < height; ++y) {
    memcpy(dst + (size_t)y * stride, src + (size_t)y * width, width);
    memset(dst + (size_t)y * stride + width, src[(size_t)y * width + width - 1], stride - width);
  }
  for (int y = height; y < padded_height; ++y) {
    memcpy(dst + (size_t)y * stride, dst + (size_t)(height - 1) * stride, stride);
  }
}

void vnt_hevc_close(vnt_hevc *ctx) {
  if (!ctx) return;
  if (ctx->encoder) ctx->api->encoder_close(ctx->encoder);
  free(ctx->output);
  free(ctx);
}

vnt_hevc *vnt_hevc_open(int width, int height, int fps, int kbps, int gop) {
  const kvz_api *api = kvz_api_get(8);
  if (!api) return NULL;
  kvz_config *cfg = api->config_alloc();
  if (!cfg) return NULL;
  vnt_hevc *ctx = NULL;
  if (!api->config_init(cfg)) goto done;
  cfg->enable_logging_output = 0;
  if (!api->config_parse(cfg, "preset", "ultrafast") ||
      !api->config_parse(cfg, "gop", "0") ||
      !api->config_parse(cfg, "open-gop", "false") ||
      !api->config_parse(cfg, "rc-algorithm", "lambda") ||
      !api->config_parse(cfg, "hash", "none")) goto done;
  cfg->width = width;
  cfg->height = height;
  cfg->framerate_num = fps;
  cfg->framerate_denom = 1;
  cfg->target_bitrate = kbps * 1000;
  cfg->intra_period = gop;
  cfg->vps_period = 1; // VPS/SPS/PPS accompany every IDR for WebCodecs recovery.
  cfg->owf = 0; // Finish this frame before returning; no delayed frames at teardown.
  cfg->threads = 4;
  cfg->vui.fullrange = 0;
  cfg->vui.colorprim = 1;
  cfg->vui.transfer = 1;
  cfg->vui.colormatrix = 1; // BGRA conversion uses limited-range BT.709.
  cfg->level = 51;
  ctx = calloc(1, sizeof(*ctx));
  if (!ctx) goto done;
  ctx->api = api;
  ctx->width = width;
  ctx->height = height;
  ctx->encoder = api->encoder_open(cfg);
  if (!ctx->encoder) {
    vnt_hevc_close(ctx);
    ctx = NULL;
  }
done:
  api->config_destroy(cfg);
  return ctx;
}

// Output memory belongs to ctx and remains valid until the next encode/close.
int vnt_hevc_encode(vnt_hevc *ctx, const uint8_t *i420, int64_t pts,
                    const uint8_t **data, uint32_t *len, int *keyframe) {
  *data = NULL;
  *len = 0;
  *keyframe = 0;
  // Kvazaar encodes blocks of at least 8 pixels; SPS crops padding on decode.
  const int pw = (ctx->width + 7) & ~7;
  const int ph = (ctx->height + 7) & ~7;
  kvz_picture *pic = ctx->api->picture_alloc(pw, ph);
  if (!pic) return 0;
  const size_t pixels = (size_t)ctx->width * ctx->height;
  copy_plane(pic->y, pic->stride, ph, i420, ctx->width, ctx->height);
  copy_plane(pic->u, pic->stride / 2, ph / 2, i420 + pixels, ctx->width / 2, ctx->height / 2);
  copy_plane(pic->v, pic->stride / 2, ph / 2, i420 + pixels + pixels / 4, ctx->width / 2, ctx->height / 2);
  pic->pts = pts;
  kvz_data_chunk *chunks = NULL;
  uint32_t length = 0;
  kvz_frame_info info = {0};
  int ok = ctx->api->encoder_encode(ctx->encoder, pic, &chunks, &length,
                                    NULL, NULL, &info);
  ctx->api->picture_free(pic);
  if (!ok || !chunks || !length) {
    ctx->api->chunk_free(chunks);
    return 0;
  }
  if (length > ctx->capacity) {
    uint8_t *buffer = realloc(ctx->output, length);
    if (!buffer) {
      ctx->api->chunk_free(chunks);
      return 0;
    }
    ctx->output = buffer;
    ctx->capacity = length;
  }
  uint32_t offset = 0;
  for (kvz_data_chunk *c = chunks; c; c = c->next) {
    if (c->len > length - offset) {
      ctx->api->chunk_free(chunks);
      return 0;
    }
    memcpy(ctx->output + offset, c->data, c->len);
    offset += c->len;
  }
  ctx->api->chunk_free(chunks);
  if (offset != length) return 0;
  *data = ctx->output;
  *len = length;
  *keyframe = info.nal_unit_type == KVZ_NAL_IDR_W_RADL ||
              info.nal_unit_type == KVZ_NAL_IDR_N_LP;
  return 1;
}
