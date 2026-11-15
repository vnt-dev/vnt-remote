// SPDX-License-Identifier: LGPL-2.1-or-later
// Replaceable Linux encoding module. No FFmpeg structs cross this ABI.
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/opt.h>
#include <libavutil/error.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>

typedef struct {
    AVCodecContext *codec;
    AVBufferRef *device;
    AVFrame *cpu;
    AVFrame *gpu;
    AVPacket *packet;
    uint8_t *output;
    size_t capacity;
    int vaapi, width, height;
} Encoder;
static int fail(char *error, size_t length, const char *what, int code) {
    char detail[AV_ERROR_MAX_STRING_SIZE] = {0};
    av_strerror(code, detail, sizeof(detail));
    if (error && length) snprintf(error, length, "%s: %s", what, detail);
    return -1;
}
unsigned vnt_hw_abi_version(void) { av_log_set_level(AV_LOG_QUIET); return 1; }
void vnt_hw_close(Encoder *e) {
    if (!e) return;
    av_packet_free(&e->packet);
    av_frame_free(&e->cpu);
    av_frame_free(&e->gpu);
    avcodec_free_context(&e->codec);
    av_buffer_unref(&e->device);
    free(e->output);
    free(e);
}
int vnt_hw_nvenc_devices(void) {
    void *cuda = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!cuda) return 0;
    int (*init)(unsigned) = dlsym(cuda,"cuInit");
    int (*count)(int*) = dlsym(cuda,"cuDeviceGetCount");
    int n=0;
    if (!init || !count || init(0) || count(&n)) n=0;
    dlclose(cuda);
    return n>32 ? 32 : n;
}
Encoder *vnt_hw_open(int codec, int vaapi, const char *device, unsigned width,
    unsigned height, unsigned fps, unsigned kbps, unsigned gop, char *error, size_t error_length) {
    const char *names[2][3]={{"h264_nvenc","hevc_nvenc","av1_nvenc"},{"h264_vaapi","hevc_vaapi","av1_vaapi"}};
    if (codec<0 || codec>2 || vaapi<0 || vaapi>1 || !device || width<2 || height<2 ||
        width>16384 || height>16384 || width%2 || height%2 || !fps || fps>120 || kbps<100 || kbps>200000) {
        fail(error,error_length,"invalid encoder parameters",AVERROR(EINVAL));return NULL;
    }
    const AVCodec *implementation=avcodec_find_encoder_by_name(names[vaapi][codec]);
    if (!implementation) { fail(error,error_length,"encoder not built",AVERROR_ENCODER_NOT_FOUND);return NULL; }
    Encoder *e=calloc(1,sizeof(*e));
    if (!e) {fail(error,error_length,"allocation",AVERROR(ENOMEM));return NULL;}
    int result=AVERROR(ENOMEM);
    e->width=width;e->height=height;e->vaapi=vaapi;
    e->codec=avcodec_alloc_context3(implementation);
    e->cpu=av_frame_alloc();e->packet=av_packet_alloc();
    if (!e->codec || !e->cpu || !e->packet) goto failed;
    AVCodecContext *c=e->codec;
    c->width=width;c->height=height;c->time_base=(AVRational){1,1000};c->framerate=(AVRational){fps,1};
    c->bit_rate=(int64_t)kbps*1000;c->gop_size=gop ? (int)(gop>INT_MAX ? INT_MAX:gop):(int)fps*2;
    c->max_b_frames=0;c->thread_count=1;c->flags|=AV_CODEC_FLAG_LOW_DELAY|AV_CODEC_FLAG_CLOSED_GOP;
    c->color_range=AVCOL_RANGE_MPEG;c->colorspace=AVCOL_SPC_BT709;c->color_primaries=AVCOL_PRI_BT709;c->color_trc=AVCOL_TRC_BT709;
    e->cpu->format=AV_PIX_FMT_NV12;e->cpu->width=width;e->cpu->height=height;
    if ((result=av_frame_get_buffer(e->cpu,32))<0) goto failed;
    if (vaapi) {
        if ((result=av_hwdevice_ctx_create(&e->device,AV_HWDEVICE_TYPE_VAAPI,device,NULL,0))<0) goto failed;
        AVBufferRef *frames=av_hwframe_ctx_alloc(e->device);
        if (!frames) {result=AVERROR(ENOMEM);goto failed;}
        AVHWFramesContext *f=(AVHWFramesContext*)frames->data;
        f->format=AV_PIX_FMT_VAAPI;f->sw_format=AV_PIX_FMT_NV12;f->width=width;f->height=height;f->initial_pool_size=4;
        result=av_hwframe_ctx_init(frames);
        if (result<0) {av_buffer_unref(&frames);goto failed;}
        c->hw_frames_ctx=frames;c->pix_fmt=AV_PIX_FMT_VAAPI;
        av_opt_set_int(c->priv_data,"async_depth",1,0);
        av_opt_set_int(c->priv_data,"idr_interval",0,0);
    } else {
        c->pix_fmt=AV_PIX_FMT_NV12;
        av_opt_set(c->priv_data,"gpu",device,0);
        av_opt_set(c->priv_data,"preset","p4",0);
        av_opt_set(c->priv_data,"tune","ull",0);
        av_opt_set_int(c->priv_data,"zerolatency",1,0);
        av_opt_set_int(c->priv_data,"delay",0,0);
        av_opt_set_int(c->priv_data,"forced-idr",1,0);
    }
    if ((result=avcodec_open2(c,implementation,NULL))<0) goto failed;
    return e;
failed:
    fail(error,error_length,"opening hardware encoder",result);
    vnt_hw_close(e);return NULL;
}
// Output remains owned by the handle until the next encode. No flush is used
// for normal frames: B-frames/lookahead are disabled, preserving live latency.
int vnt_hw_encode(Encoder *e,const uint8_t *nv12,size_t length,uint64_t pts,int key,
    const uint8_t **output,size_t *output_length,int *keyframe,char *error,size_t error_length) {
    *output=NULL;*output_length=0;*keyframe=0;
    if (!e || !nv12 || length!=(size_t)e->width*e->height*3/2 || pts>INT64_MAX)
        return fail(error,error_length,"invalid frame",AVERROR(EINVAL));
    int result=av_frame_make_writable(e->cpu);
    if (result<0) return fail(error,error_length,"writable frame",result);
    for (int y=0;y<e->height;y++) memcpy(e->cpu->data[0]+y*e->cpu->linesize[0],nv12+(size_t)y*e->width,e->width);
    for (int y=0;y<e->height/2;y++) memcpy(e->cpu->data[1]+y*e->cpu->linesize[1],nv12+(size_t)e->width*e->height+(size_t)y*e->width,e->width);
    AVFrame *frame=e->cpu;
    if (e->vaapi) {
        av_frame_free(&e->gpu);e->gpu=av_frame_alloc();
        if (!e->gpu) return fail(error,error_length,"GPU frame",AVERROR(ENOMEM));
        if ((result=av_hwframe_get_buffer(e->codec->hw_frames_ctx,e->gpu,0))<0 ||
            (result=av_hwframe_transfer_data(e->gpu,e->cpu,0))<0)
            return fail(error,error_length,"upload frame",result);
        frame=e->gpu;
    }
    frame->pts=pts;frame->pict_type=key ? AV_PICTURE_TYPE_I:AV_PICTURE_TYPE_NONE;
    frame->color_range=e->codec->color_range;frame->colorspace=e->codec->colorspace;
    frame->color_primaries=e->codec->color_primaries;frame->color_trc=e->codec->color_trc;
    if ((result=avcodec_send_frame(e->codec,frame))<0) return fail(error,error_length,"send frame",result);
    size_t used=0;
    while ((result=avcodec_receive_packet(e->codec,e->packet))>=0) {
        if (e->packet->pts != (int64_t)pts) {av_packet_unref(e->packet);return fail(error,error_length,"unexpected delayed frame",AVERROR_INVALIDDATA);}
        size_t bytes=(size_t)e->packet->size;
        if (bytes>32*1024*1024 || used>32*1024*1024-bytes) {av_packet_unref(e->packet);return fail(error,error_length,"packet too large",AVERROR_INVALIDDATA);}
        if (used+bytes>e->capacity) {
            uint8_t *allocation=realloc(e->output,used+bytes);
            if (!allocation) {av_packet_unref(e->packet);return fail(error,error_length,"packet allocation",AVERROR(ENOMEM));}
            e->output=allocation;e->capacity=used+bytes;
        }
        memcpy(e->output+used,e->packet->data,bytes);used+=bytes;
        *keyframe|=(e->packet->flags&AV_PKT_FLAG_KEY)!=0;
        av_packet_unref(e->packet);
    }
    if (result!=AVERROR(EAGAIN)) return fail(error,error_length,"receive frame",result);
    if (!used) return fail(error,error_length,"encoder buffered a low-delay frame",AVERROR(EAGAIN));
    *output=e->output;*output_length=used;return 0;
}
