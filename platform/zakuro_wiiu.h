/* the interface between the Wii U platform layer (C, SDL2 + wut) and the
   Zakuro runtime (Rust, runtime/src/lib.rs). the C side owns the main
   thread, the screens, sound and the controllers; the runtime runs the
   console on a thread of its own and hands frames and sound back. */

#ifndef ZAKURO_WIIU_H
#define ZAKURO_WIIU_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* the console's buttons, the bits of zakuro_core's PadState */
enum {
    ZW_BTN_A = 1u << 0,
    ZW_BTN_B = 1u << 1,
    ZW_BTN_SELECT = 1u << 2,
    ZW_BTN_START = 1u << 3,
    ZW_BTN_RIGHT = 1u << 4,
    ZW_BTN_LEFT = 1u << 5,
    ZW_BTN_UP = 1u << 6,
    ZW_BTN_DOWN = 1u << 7,
    ZW_BTN_R = 1u << 8,
    ZW_BTN_L = 1u << 9,
    ZW_BTN_X = 1u << 10,
    ZW_BTN_Y = 1u << 11,
    ZW_BTN_ZL = 1u << 14,
    ZW_BTN_ZR = 1u << 15,
};

/* the screens' sizes, upright */
#define ZW_TOP_WIDTH 400
#define ZW_BOTTOM_WIDTH 320
#define ZW_SCREEN_HEIGHT 240

/* what the controllers say, each poll. sticks are -1..1, up positive. */
typedef struct ZwInput {
    uint32_t buttons;
    float circle_x, circle_y;
    float c_stick_x, c_stick_y;
    uint8_t touching;
    uint8_t pad_[1];
    uint16_t touch_x, touch_y; /* bottom screen pixels, 0..319, 0..239 */
    uint16_t pad2_;
} ZwInput;

/* the recompiled code linked into the program, the recomp_ symbols
   3dsrecomp generated, or none to interpret everything. */
typedef struct ZwCode {
    uint64_t program_id;
    uint32_t abi;
    uint32_t generation;
    const void *entries;
    uint32_t entry_count;
    const void *modules;
    uint32_t module_count;
    const void *origins;
    uint8_t present;
    uint8_t pad_[3];
} ZwCode;

typedef struct ZwParams {
    /* the decrypted game, a .cxi, .3ds or .cia */
    const char *game_path;
    /* saves and Zakuro's own files */
    const char *data_dir;
    ZwCode code;
    uint8_t new3ds;
    /* 0 Japanese, 1 English, 2 French, 3 German, 4 Italian, 5 Spanish... 0xFF the default */
    uint8_t language;
    /* 0 Japan, 1 USA, 2 Europe, 0xFF the default */
    uint8_t region;
    /* 0 draws the 3D on the CPU, 1 on the GPU through GX2 when
       zw_gx2_available(), see zw_gx2.h */
    uint8_t renderer;
    /* the CPU core the console runs on, 0..2, or -1 for any */
    int32_t emu_core;
} ZwParams;

enum {
    ZW_STATE_LOADING = 0,
    ZW_STATE_RUNNING = 1,
    ZW_STATE_EXITED = 2,
    ZW_STATE_FAULTED = 3,
    ZW_STATE_FAILED = 4,
};

/* runtime side (Rust) */
int32_t zakuro_wiiu_start(const ZwParams *params);
void zakuro_wiiu_set_input(const ZwInput *input);
/* copies the newest frame, RGBA8 bytes, rows top to bottom: top is
   400x240, bottom 320x240. 1 when there was one not taken yet. */
int32_t zakuro_wiiu_take_frame(uint8_t *top, uint8_t *bottom);
/* interleaved stereo frames at zakuro_wiiu_audio_rate(), up to max. */
uint32_t zakuro_wiiu_take_audio(int16_t *out, uint32_t max_frames);
uint32_t zakuro_wiiu_audio_rate(void);
int32_t zakuro_wiiu_state(void);
/* the state's message, a load error or the fault, into buffer. */
uint32_t zakuro_wiiu_message(char *buffer, uint32_t size);
/* the main thread is alive and in the foreground; the console pauses when
   this stops coming, as it does in the HOME menu. */
void zakuro_wiiu_heartbeat(void);
/* holds the console still (1) or lets it go on (0), for the HOME menu */
void zakuro_wiiu_pause(int32_t paused);
/* the console's frames so far, for the frame rate */
uint32_t zakuro_wiiu_frames(void);
/* where the console's time went over the last second, one line with a '|'
   where it may be split in two, into buffer; its full length */
uint32_t zakuro_wiiu_perf(char *buffer, uint32_t size);
void zakuro_wiiu_stop(void);

/* platform side (C), called by the runtime */
void zw_platform_log(int32_t level, const char *message);
/* called first thing on the console's thread */
void zw_platform_thread_setup(int32_t core);
/* called first thing on each of the GPU's worker threads, index 0 or 1 */
void zw_platform_worker_setup(int32_t index);

#ifdef __cplusplus
}
#endif

#endif
