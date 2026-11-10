/* the Wii U side of a recompiled 3DS game: SDL2 draws the console's two
   screens on the TV and the GamePad in one of three layouts, plays its
   sound and reads the controllers; the Zakuro runtime (Rust) runs the
   console on a thread of its own.

   Right stick click (R3) on the GamePad or a Pro Controller cycles the
   layouts; holding L + ZL + R + ZR + Minus shows or hides the frame rate.
   both choices are kept per game on the SD card, in wiiu.cfg, next to
   log.txt, which has what the program and the console said, the last run's
   in log-previous.txt. */

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#include <SDL.h>
#include <SDL_test_font.h>

#include <coreinit/core.h>
#include <coreinit/mutex.h>
#include <coreinit/thread.h>
#include <coreinit/time.h>
#include <padscore/kpad.h>
#include <padscore/wpad.h>
#include <vpad/input.h>
#include <whb/log.h>
#include <whb/log_cafe.h>
#include <whb/log_udp.h>

#include "game_info.h"
#include "layout.h"
#include "zakuro_wiiu.h"
#include "zw_gx2.h"

#define SD_ROOT "fs:/vol/external01/wiiu/zakuro"
#define CONTENT_ROOT "fs:/vol/content"
#define DRC_WIDTH 854
#define DRC_HEIGHT 480
#define STICK_DEADZONE 0.15f
/* sound held for the output, past which more is dropped: about 100 ms */
#define AUDIO_MOST_QUEUED_MS 100
#define TOAST_FRAMES 150
/* when the GPU's work for the console runs between frames: from one
   present to this long after it, leaving the rest of a 59.94 Hz frame for
   drawing the screens. measured from the present, which returns at vsync
   when it waits, so the window follows the display */
#define SERVICE_US 14000
/* the log file stops growing past this */
#define LOG_MOST_BYTES (8u << 20)
/* the buttons, all held, that show or hide the frame rate */
#define FPS_COMBO (ZW_BTN_L | ZW_BTN_ZL | ZW_BTN_R | ZW_BTN_ZR | ZW_BTN_SELECT)

static uint8_t top_pixels[ZW_TOP_WIDTH * ZW_SCREEN_HEIGHT * 4];
static uint8_t bottom_pixels[ZW_BOTTOM_WIDTH * ZW_SCREEN_HEIGHT * 4];

typedef struct Display {
    SDL_Window *window;
    SDL_Renderer *renderer;
    SDL_Texture *top, *bottom;
    int width, height;
} Display;

static Display tv, drc;
static int layout = ZW_LAYOUT_SPLIT;
/* where the 3D is drawn: 1 the GPU through GX2 when the program has the
   shaders, 0 the CPU */
static int renderer = 1;
/* the frame rate in a corner, and the log file */
static int show_fps;
static int log_enabled = 1;
/* the core the console's thread runs on. core 1, the main thread's, has
   2 MiB of L2 cache to the others' 512 KiB, which a big game's recompiled
   code (tens of MiB of it) needs; the console runs under the main thread
   there, which mostly waits for the display and the console's GX2 batches */
static int console_core = 1;
static int main_priority = 16;
static char settings_path[256];
static char toast[96];
static int toast_frames;

/* ---- the log file ----

   every line WHBLog prints (the platform's and the runtime's) goes to
   log.txt on the SD card as well, after the time since the start. lines
   wait in a buffer the main loop writes out each second; warnings and
   errors are written at once, so a crash right after one leaves it there. */

static FILE *log_file;
static OSMutex log_mutex;
static OSTime log_start;
static uint32_t log_written;
static int log_dirty;

static void log_to_file(const char *message) {
    OSLockMutex(&log_mutex);
    if (log_file && log_written < LOG_MOST_BYTES) {
        uint32_t ms = (uint32_t)OSTicksToMilliseconds(OSGetSystemTime() - log_start);
        size_t length = strlen(message);
        int newline = length > 0 && message[length - 1] == '\n';
        int count = fprintf(log_file, "[%5u.%03u] %s%s", ms / 1000, ms % 1000, message, newline ? "" : "\n");
        if (count > 0) log_written += (uint32_t)count;
        if (log_written >= LOG_MOST_BYTES) fputs("the log is full, nothing more is written\n", log_file);
        log_dirty = 1;
    }
    OSUnlockMutex(&log_mutex);
}

static void log_flush(void) {
    OSLockMutex(&log_mutex);
    if (log_file && log_dirty) {
        fflush(log_file);
        log_dirty = 0;
    }
    OSUnlockMutex(&log_mutex);
}

/* the last run's log is kept as log-previous.txt */
static void open_log(const char *title_dir) {
    static char buffer[32 * 1024];
    char path[256], previous[256];
    snprintf(path, sizeof path, "%s/log.txt", title_dir);
    snprintf(previous, sizeof previous, "%s/log-previous.txt", title_dir);
    remove(previous);
    rename(path, previous);
    log_start = OSGetSystemTime();
    log_file = fopen(path, "w");
    if (!log_file) {
        WHBLogPrintf("could not write %s (%d)", path, errno);
        return;
    }
    setvbuf(log_file, buffer, _IOFBF, sizeof buffer);
    WHBAddLogHandler(log_to_file);
}

static void close_log(void) {
    if (!log_file) return;
    WHBRemoveLogHandler(log_to_file);
    OSLockMutex(&log_mutex);
    fclose(log_file);
    log_file = NULL;
    OSUnlockMutex(&log_mutex);
}

/* ---- the runtime's calls into the platform ---- */

void zw_platform_log(int32_t level, const char *message) {
    WHBLogPrint(message);
    /* 1 errors, 2 warnings */
    if (level <= 2) log_flush();
}

static const uint32_t core_affinity[3] = {
    OS_THREAD_ATTRIB_AFFINITY_CPU0, OS_THREAD_ATTRIB_AFFINITY_CPU1, OS_THREAD_ATTRIB_AFFINITY_CPU2,
};

void zw_platform_thread_setup(int32_t core) {
    OSThread *thread = OSGetCurrentThread();
    if (core >= 0 && core <= 2) {
        OSSetThreadAffinity(thread, core_affinity[core]);
        /* sharing the main thread's core, the main thread comes first (lower
           numbers do): it runs GX2 batches the console waits on, and the
           screens */
        if (core == 1) OSSetThreadPriority(thread, main_priority + 1);
        /* a running thread moves at its next reschedule */
        OSYieldThread();
        WHBLogPrintf("console thread on core %u (asked for %d), priority %d", OSGetCoreId(), core, OSGetThreadPriority(thread));
    }
}

void zw_platform_worker_setup(int32_t index) {
    OSThread *thread = OSGetCurrentThread();
    /* the first on core 0, which is otherwise idle; the second on the core
       the console doesn't use of 1 and 2, below the main thread and the
       console (lower numbers come first), so they go before it */
    if (index == 0) {
        OSSetThreadAffinity(thread, OS_THREAD_ATTRIB_AFFINITY_CPU0);
    } else {
        OSSetThreadAffinity(thread, core_affinity[console_core == 1 ? 2 : 1]);
        OSSetThreadPriority(thread, main_priority + 4);
    }
    OSYieldThread();
    WHBLogPrintf("GPU worker %d on core %u, priority %d", (int)index, OSGetCoreId(), OSGetThreadPriority(thread));
}

/* ---- settings ---- */

static void mkdirs(const char *path) {
    char partial[256];
    size_t length = strlen(path);
    if (length >= sizeof partial) return;
    memcpy(partial, path, length + 1);
    /* past "fs:/vol/external01/" which exists */
    for (size_t i = sizeof "fs:/vol/external01/" - 1; i < length; i++) {
        if (partial[i] == '/') {
            partial[i] = 0;
            mkdir(partial, 0777);
            partial[i] = '/';
        }
    }
    mkdir(partial, 0777);
}

static void load_settings(void) {
    layout = zw_game_default_layout;
    renderer = zw_game_renderer ? 1 : 0;
    FILE *file = fopen(settings_path, "r");
    if (!file) return;
    char line[128];
    while (fgets(line, sizeof line, file)) {
        int value;
        if (sscanf(line, "layout=%d", &value) == 1 && value >= 0 && value < ZW_LAYOUT_COUNT) {
            layout = value;
        } else if (sscanf(line, "renderer=%d", &value) == 1 && (value == 0 || value == 1)) {
            renderer = value;
        } else if (sscanf(line, "fps=%d", &value) == 1) {
            show_fps = value != 0;
        } else if (sscanf(line, "log=%d", &value) == 1) {
            log_enabled = value != 0;
        } else if (sscanf(line, "core=%d", &value) == 1 && value >= 0 && value <= 2) {
            console_core = value;
        }
    }
    fclose(file);
}

static void save_settings(void) {
    FILE *file = fopen(settings_path, "w");
    if (!file) {
        WHBLogPrintf("could not write %s (%d)", settings_path, errno);
        return;
    }
    fprintf(file,
            "# Zakuro for Wii U, %s\n# layout: 0 top on TV + bottom on GamePad, 1 both on TV, 2 both on GamePad\nlayout=%d\n"
            "# renderer: 1 draws the 3D on the GPU (GX2), 0 on the CPU; read at start\nrenderer=%d\n"
            "# fps: 1 shows the frame rate (L + ZL + R + ZR + Minus switches it)\nfps=%d\n"
            "# log: 1 writes log.txt next to this file; read at start\nlog=%d\n"
            "# core: the CPU core the game runs on, 1 (the most cache) or 0 or 2; read at start\ncore=%d\n",
            zw_game_title, layout, renderer, show_fps, log_enabled, console_core);
    fclose(file);
}

static void show_toast(const char *text) {
    snprintf(toast, sizeof toast, "%s", text);
    toast_frames = TOAST_FRAMES;
}

/* ---- the controllers ---- */

static float deadzone(float value) {
    return (value > -STICK_DEADZONE && value < STICK_DEADZONE) ? 0.0f : value;
}

typedef struct Mapping {
    uint32_t from, to;
} Mapping;

static const Mapping vpad_map[] = {
    {VPAD_BUTTON_A, ZW_BTN_A},         {VPAD_BUTTON_B, ZW_BTN_B},
    {VPAD_BUTTON_X, ZW_BTN_X},         {VPAD_BUTTON_Y, ZW_BTN_Y},
    {VPAD_BUTTON_L, ZW_BTN_L},         {VPAD_BUTTON_R, ZW_BTN_R},
    {VPAD_BUTTON_ZL, ZW_BTN_ZL},       {VPAD_BUTTON_ZR, ZW_BTN_ZR},
    {VPAD_BUTTON_PLUS, ZW_BTN_START},  {VPAD_BUTTON_MINUS, ZW_BTN_SELECT},
    {VPAD_BUTTON_UP, ZW_BTN_UP},       {VPAD_BUTTON_DOWN, ZW_BTN_DOWN},
    {VPAD_BUTTON_LEFT, ZW_BTN_LEFT},   {VPAD_BUTTON_RIGHT, ZW_BTN_RIGHT},
};

/* the Pro Controller and the Classic Controller share their bits */
static const Mapping pro_map[] = {
    {WPAD_PRO_BUTTON_A, ZW_BTN_A},         {WPAD_PRO_BUTTON_B, ZW_BTN_B},
    {WPAD_PRO_BUTTON_X, ZW_BTN_X},         {WPAD_PRO_BUTTON_Y, ZW_BTN_Y},
    {WPAD_PRO_TRIGGER_L, ZW_BTN_L},        {WPAD_PRO_TRIGGER_R, ZW_BTN_R},
    {WPAD_PRO_TRIGGER_ZL, ZW_BTN_ZL},      {WPAD_PRO_TRIGGER_ZR, ZW_BTN_ZR},
    {WPAD_PRO_BUTTON_PLUS, ZW_BTN_START},  {WPAD_PRO_BUTTON_MINUS, ZW_BTN_SELECT},
    {WPAD_PRO_BUTTON_UP, ZW_BTN_UP},       {WPAD_PRO_BUTTON_DOWN, ZW_BTN_DOWN},
    {WPAD_PRO_BUTTON_LEFT, ZW_BTN_LEFT},   {WPAD_PRO_BUTTON_RIGHT, ZW_BTN_RIGHT},
};

static uint32_t map_buttons(uint32_t held, const Mapping *map, size_t count) {
    uint32_t out = 0;
    for (size_t i = 0; i < count; i++) {
        if (held & map[i].from) out |= map[i].to;
    }
    return out;
}

static void add_stick(float *x, float *y, float sx, float sy) {
    sx = deadzone(sx);
    sy = deadzone(sy);
    /* the stick leaning furthest wins */
    if (sx * sx + sy * sy > *x * *x + *y * *y) {
        *x = sx;
        *y = sy;
    }
}

static VPADStatus vpad;
static int vpad_valid;
/* each Wii Remote channel's last sample, kept through polls with none new */
static KPADStatus kpad[4];
static int kpad_valid[4];
/* the last touch coordinate the panel vouched for, per axis */
static float touch_last_x, touch_last_y;

/* polls every controller; *cycle_layout is set on a fresh R3 press,
   *toggle_fps when L + ZL + R + ZR + Minus come to be held together */
static void poll_input(ZwInput *input, const ZwLayoutPlan *plan, int *cycle_layout, int *toggle_fps) {
    static int combo_held;
    static uint32_t last_layout_buttons;
    uint32_t layout_buttons = 0;
    VPADReadError error = VPAD_READ_NO_SAMPLES;
    VPADStatus fresh;

    memset(input, 0, sizeof *input);
    *cycle_layout = 0;
    *toggle_fps = 0;

    if (VPADRead(VPAD_CHAN_0, &fresh, 1, &error) > 0 && error == VPAD_READ_SUCCESS) {
        vpad = fresh;
        vpad_valid = 1;
    } else if (error == VPAD_READ_INVALID_CONTROLLER) {
        vpad_valid = 0;
    }
    if (vpad_valid) {
        input->buttons |= map_buttons(vpad.hold, vpad_map, sizeof vpad_map / sizeof *vpad_map);
        add_stick(&input->circle_x, &input->circle_y, vpad.leftStick.x, vpad.leftStick.y);
        add_stick(&input->c_stick_x, &input->c_stick_y, vpad.rightStick.x, vpad.rightStick.y);
        if (vpad.hold & VPAD_BUTTON_STICK_R) layout_buttons |= 1;

        VPADTouchData touch;
        VPADGetTPCalibratedPoint(VPAD_CHAN_0, &touch, &vpad.tpNormal);
        if (touch.touched) {
            /* calibrated points come in a 1280 x 720 space over the panel; an
               axis the panel flags invalid, as it does near the edges, keeps
               its last good value */
            if (!(touch.validity & VPAD_INVALID_X)) touch_last_x = touch.x * (float)DRC_WIDTH / 1280.0f;
            if (!(touch.validity & VPAD_INVALID_Y)) touch_last_y = touch.y * (float)DRC_HEIGHT / 720.0f;
            float px = touch_last_x, py = touch_last_y;
            if (zw_layout_touch(plan, DRC_WIDTH, DRC_HEIGHT, px, py, &input->touch_x, &input->touch_y)) {
                input->touching = 1;
            }
        }
    }

    for (int channel = 0; channel < 4; channel++) {
        KPADStatus fresh_status;
        KPADError kerror = KPAD_ERROR_OK;
        memset(&fresh_status, 0, sizeof fresh_status);
        uint32_t read = KPADReadEx((KPADChan)channel, &fresh_status, 1, &kerror);
        if (read > 0 && kerror == KPAD_ERROR_OK) {
            kpad[channel] = fresh_status;
            kpad_valid[channel] = 1;
        } else if (kerror == KPAD_ERROR_INVALID_CONTROLLER || kerror == KPAD_ERROR_UNINITIALIZED) {
            kpad_valid[channel] = 0;
        }
        /* no new sample: the last one still holds */
        if (!kpad_valid[channel]) continue;
        const KPADStatus status = kpad[channel];
        if (status.extensionType == WPAD_EXT_PRO_CONTROLLER) {
            input->buttons |= map_buttons(status.pro.hold, pro_map, sizeof pro_map / sizeof *pro_map);
            add_stick(&input->circle_x, &input->circle_y, status.pro.leftStick.x, status.pro.leftStick.y);
            add_stick(&input->c_stick_x, &input->c_stick_y, status.pro.rightStick.x, status.pro.rightStick.y);
            if (status.pro.hold & WPAD_PRO_BUTTON_STICK_R) layout_buttons |= 2u << channel;
        } else if (status.extensionType == WPAD_EXT_CLASSIC || status.extensionType == WPAD_EXT_MPLUS_CLASSIC) {
            /* WPAD_CLASSIC_BUTTON_* share the Pro Controller's bits */
            input->buttons |= map_buttons(status.classic.hold, pro_map, sizeof pro_map / sizeof *pro_map);
            add_stick(&input->circle_x, &input->circle_y, status.classic.leftStick.x, status.classic.leftStick.y);
            add_stick(&input->c_stick_x, &input->c_stick_y, status.classic.rightStick.x, status.classic.rightStick.y);
        }
    }

    if (layout_buttons & ~last_layout_buttons) *cycle_layout = 1;
    last_layout_buttons = layout_buttons;

    /* across controllers, as the console sees them; while all five are
       held the game sees none of them, so the combination does nothing
       there */
    int combo = (input->buttons & FPS_COMBO) == FPS_COMBO;
    *toggle_fps = combo && !combo_held;
    combo_held = combo;
    if (combo) input->buttons &= ~FPS_COMBO;
}

/* ---- the screens ---- */

static int open_display(Display *display, const char *name, uint32_t only, int width, int height) {
    display->window = SDL_CreateWindow(name, SDL_WINDOWPOS_UNDEFINED, SDL_WINDOWPOS_UNDEFINED, width, height, only);
    if (!display->window) {
        WHBLogPrintf("could not open the %s window: %s", name, SDL_GetError());
        return -1;
    }
    /* only the TV's present waits for the display: the GamePad's swaps
       with it (PREVENT_SWAP), and waiting for vsync on both halves the
       loop to 30 Hz */
    uint32_t flags = SDL_RENDERER_ACCELERATED | ((only & SDL_WINDOW_WIIU_GAMEPAD_ONLY) ? 0 : SDL_RENDERER_PRESENTVSYNC);
    display->renderer = SDL_CreateRenderer(display->window, -1, flags);
    if (!display->renderer) {
        WHBLogPrintf("could not draw on the %s: %s", name, SDL_GetError());
        return -1;
    }
    if (SDL_GetRendererOutputSize(display->renderer, &display->width, &display->height) != 0 || display->width <= 0) {
        display->width = width;
        display->height = height;
    }
    /* RGBX8888 is a 32-bit word 0xRRGGBBXX, on this big-endian CPU bytes
       R, G, B, X in memory, how the runtime hands the screens over. The X
       matters: the console's framebuffers carry whatever alpha the game
       left, which its LCD ignores, while SDL would blend by it and its Wii U
       renderer alpha-tests every texture, so a game that leaves alpha at 0
       would show black. RGBX reads alpha as 1 and draws without blending. */
    display->top = SDL_CreateTexture(display->renderer, SDL_PIXELFORMAT_RGBX8888, SDL_TEXTUREACCESS_STREAMING, ZW_TOP_WIDTH, ZW_SCREEN_HEIGHT);
    display->bottom = SDL_CreateTexture(display->renderer, SDL_PIXELFORMAT_RGBX8888, SDL_TEXTUREACCESS_STREAMING, ZW_BOTTOM_WIDTH, ZW_SCREEN_HEIGHT);
    if (!display->top || !display->bottom) {
        WHBLogPrintf("could not make the %s's textures: %s", name, SDL_GetError());
        return -1;
    }
    SDL_SetTextureScaleMode(display->top, SDL_ScaleModeLinear);
    SDL_SetTextureScaleMode(display->bottom, SDL_ScaleModeLinear);
    SDL_SetTextureBlendMode(display->top, SDL_BLENDMODE_NONE);
    SDL_SetTextureBlendMode(display->bottom, SDL_BLENDMODE_NONE);
    /* texture memory starts as whatever was there: black until a frame */
    SDL_UpdateTexture(display->top, NULL, top_pixels, ZW_TOP_WIDTH * 4);
    SDL_UpdateTexture(display->bottom, NULL, bottom_pixels, ZW_BOTTOM_WIDTH * 4);
    WHBLogPrintf("%s is %dx%d", name, display->width, display->height);
    return 0;
}

static void close_display(Display *display) {
    if (display->top) SDL_DestroyTexture(display->top);
    if (display->bottom) SDL_DestroyTexture(display->bottom);
    if (display->renderer) SDL_DestroyRenderer(display->renderer);
    if (display->window) SDL_DestroyWindow(display->window);
    memset(display, 0, sizeof *display);
}

static void blit(Display *display, SDL_Texture *texture, ZwRect rect) {
    if (rect.w <= 0) return;
    SDL_Rect to = {rect.x, rect.y, rect.w, rect.h};
    SDL_RenderCopy(display->renderer, texture, NULL, &to);
}

/* a line of text, 8x8 per character, scaled up, with a dark band behind */
static void draw_text(Display *display, const char *text, int y, int scale) {
    int width = (int)strlen(text) * 8 * scale;
    int x = (display->width - width) / 2;
    SDL_Rect band = {0, y - 4 * scale, display->width, 16 * scale};
    SDL_SetRenderDrawBlendMode(display->renderer, SDL_BLENDMODE_BLEND);
    SDL_SetRenderDrawColor(display->renderer, 0, 0, 0, 180);
    SDL_RenderFillRect(display->renderer, &band);
    SDL_RenderSetScale(display->renderer, (float)scale, (float)scale);
    SDL_SetRenderDrawColor(display->renderer, 255, 255, 255, 255);
    SDLTest_DrawString(display->renderer, x / scale, y / scale, text);
    SDL_RenderSetScale(display->renderer, 1.0f, 1.0f);
}

/* a long message wrapped over several lines */
static void draw_message(Display *display, const char *message, int scale) {
    int per_line = display->width / (8 * scale) - 4;
    if (per_line < 10) per_line = 10;
    int length = (int)strlen(message);
    int lines = (length + per_line - 1) / per_line;
    int y = (display->height - lines * 12 * scale) / 2;
    char line[256];
    for (int start = 0; start < length && start < 2000; start += per_line) {
        int count = length - start < per_line ? length - start : per_line;
        if (count >= (int)sizeof line) count = sizeof line - 1;
        memcpy(line, message + start, count);
        line[count] = 0;
        draw_text(display, line, y, scale);
        y += 12 * scale;
    }
}

/* lines of text in the top left corner, over a dark box */
static void draw_corner(Display *display, const char *const *lines, int count, int scale) {
    int widest = 0;
    for (int i = 0; i < count; i++) {
        int width = (int)strlen(lines[i]);
        if (width > widest) widest = width;
    }
    if (widest == 0) return;
    SDL_Rect box = {0, 0, widest * 8 * scale + 8 * scale, count * 10 * scale + 6 * scale};
    SDL_SetRenderDrawBlendMode(display->renderer, SDL_BLENDMODE_BLEND);
    SDL_SetRenderDrawColor(display->renderer, 0, 0, 0, 170);
    SDL_RenderFillRect(display->renderer, &box);
    SDL_RenderSetScale(display->renderer, (float)scale, (float)scale);
    SDL_SetRenderDrawColor(display->renderer, 255, 255, 96, 255);
    for (int i = 0; i < count; i++) SDLTest_DrawString(display->renderer, 4, 4 + i * 10, lines[i]);
    SDL_RenderSetScale(display->renderer, 1.0f, 1.0f);
}

/* the frame rate overlay's lines, refreshed each second */
static char fps_lines[3][160];
static int fps_line_count;

static void draw(Display *display, ZwRect top, ZwRect bottom, const char *message) {
    SDL_SetRenderDrawColor(display->renderer, 0, 0, 0, 255);
    SDL_RenderClear(display->renderer);
    blit(display, display->top, top);
    blit(display, display->bottom, bottom);
    int scale = display->height >= 720 ? 3 : 2;
    if (message) draw_message(display, message, scale);
    if (toast_frames > 0) draw_text(display, toast, display->height - 24 * scale, scale);
    if (show_fps && fps_line_count > 0) {
        const char *lines[3] = {fps_lines[0], fps_lines[1], fps_lines[2]};
        draw_corner(display, lines, fps_line_count, display->height >= 720 ? 2 : 1);
    }
    SDL_RenderPresent(display->renderer);
}

/* ---- sound ---- */

static SDL_AudioDeviceID audio;
static uint32_t audio_rate;
static int audio_started;

static void open_audio(void) {
    SDL_AudioSpec want, have;
    SDL_zero(want);
    audio_rate = zakuro_wiiu_audio_rate();
    want.freq = (int)audio_rate;
    want.format = AUDIO_S16SYS;
    want.channels = 2;
    want.samples = 1024;
    want.callback = NULL;
    /* SDL converts to what the hardware takes, 48 kHz */
    audio = SDL_OpenAudioDevice(NULL, 0, &want, &have, 0);
    if (!audio) WHBLogPrintf("no sound: %s", SDL_GetError());
}

static void pump_audio(void) {
    static int16_t samples[4096 * 2];
    if (!audio) return;
    uint32_t most = audio_rate * AUDIO_MOST_QUEUED_MS / 1000 * 4;
    for (;;) {
        uint32_t frames = zakuro_wiiu_take_audio(samples, 4096);
        if (frames == 0) break;
        /* the output fell behind: what doesn't fit goes */
        if (SDL_GetQueuedAudioSize(audio) + frames * 4 <= most) SDL_QueueAudio(audio, samples, frames * 4);
    }
    if (!audio_started && SDL_GetQueuedAudioSize(audio) >= audio_rate / 20 * 4) {
        SDL_PauseAudioDevice(audio, 0);
        audio_started = 1;
    }
}

/* back from the HOME menu: what was queued and held is stale, and the
   output starts again once it has a cushion */
static void reset_audio(void) {
    static int16_t samples[4096 * 2];
    if (!audio) return;
    SDL_PauseAudioDevice(audio, 1);
    SDL_ClearQueuedAudio(audio);
    while (zakuro_wiiu_take_audio(samples, 4096) > 0) {}
    audio_started = 0;
}

/* ---- the frame rate ---- */

/* the overlay's lines, from what the last second did: the game's frames
   (the console's, what matters), the screens' presents, and the runtime's
   account of where the console's time went */
static void update_fps(OSTime elapsed, uint32_t presents) {
    static uint32_t last_frames;
    uint32_t frames = zakuro_wiiu_frames();
    float seconds = (float)OSTicksToMicroseconds(elapsed) / 1e6f;
    if (seconds <= 0.0f) return;
    float game = (float)(frames - last_frames) / seconds;
    last_frames = frames;
    snprintf(fps_lines[0], sizeof fps_lines[0], "%.1f FPS  (screens %.0f, game on core %d)", game, (float)presents / seconds, console_core);
    fps_line_count = 1;
    char perf[sizeof fps_lines[1] * 2];
    perf[0] = 0;
    zakuro_wiiu_perf(perf, sizeof perf);
    /* the runtime's account, split in two lines at the '|' it leaves */
    if (perf[0]) {
        char *split = strchr(perf, '|');
        if (split) *split = 0;
        snprintf(fps_lines[1], sizeof fps_lines[1], "%s", perf);
        fps_line_count = 2;
        if (split && split[1]) {
            snprintf(fps_lines[2], sizeof fps_lines[2], "%s", split[1] == ' ' ? split + 2 : split + 1);
            fps_line_count = 3;
        }
    }
}

/* ---- the game ---- */

static int file_exists(const char *path) {
    struct stat st;
    return stat(path, &st) == 0 && S_ISREG(st.st_mode);
}

/* the game sits in the bundle's content, or on the SD card for a bundle
   made without it */
static int find_game(char *path, size_t size, const char *title_dir) {
    snprintf(path, size, "%s/%s", CONTENT_ROOT, zw_game_rom_name);
    if (file_exists(path)) return 1;
    snprintf(path, size, "%s/%s", title_dir, zw_game_rom_name);
    if (file_exists(path)) return 1;
    return 0;
}

int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    OSInitMutex(&log_mutex);
    WHBLogCafeInit();
    WHBLogUdpInit();

    char title_dir[160], game_path[256];
    snprintf(title_dir, sizeof title_dir, "%s/%016llX", SD_ROOT, (unsigned long long)zw_game_program_id);
    mkdirs(title_dir);
    snprintf(settings_path, sizeof settings_path, "%s/wiiu.cfg", title_dir);
    load_settings();
    if (log_enabled) open_log(title_dir);
    WHBLogPrintf("Zakuro for Wii U: %s (%016llX), built " __DATE__ " " __TIME__, zw_game_title, (unsigned long long)zw_game_program_id);
    main_priority = OSGetThreadPriority(OSGetCurrentThread());
    WHBLogPrintf("settings: layout %d, renderer %s, fps %s, core %d; main thread on core %u, priority %d", layout, renderer ? "GX2" : "CPU",
                 show_fps ? "on" : "off", console_core, OSGetCoreId(), main_priority);

    if (SDL_Init(SDL_INIT_VIDEO | SDL_INIT_AUDIO) != 0) {
        WHBLogPrintf("SDL_Init: %s", SDL_GetError());
        return 1;
    }
    SDL_SetHint(SDL_HINT_RENDER_SCALE_QUALITY, "1");
    /* the TV window at the TV's resolution, 1080p sharp where it can be */
    SDL_DisplayMode tv_mode;
    SDL_zero(tv_mode);
    if (SDL_GetDesktopDisplayMode(0, &tv_mode) != 0) SDL_zero(tv_mode);
    /* the GamePad's window presents first and leaves the swap to the TV's,
       which swaps both */
    if (open_display(&drc, "GamePad", SDL_WINDOW_WIIU_GAMEPAD_ONLY | SDL_WINDOW_WIIU_PREVENT_SWAP, DRC_WIDTH, DRC_HEIGHT) != 0 ||
        open_display(&tv, "TV", SDL_WINDOW_WIIU_TV_ONLY, tv_mode.w > 0 ? tv_mode.w : 1280, tv_mode.h > 0 ? tv_mode.h : 720) != 0) {
        close_display(&tv);
        close_display(&drc);
        SDL_Quit();
        return 1;
    }
    /* the GPU's work for the console runs on this thread, GX2's, between
       SDL's frames */
    int gx2 = renderer == 1 && zw_gx2_init();
    open_audio();
    KPADInit();
    WPADEnableURCC(TRUE);

    char message[512] = "";
    int started = 0;
    if (!find_game(game_path, sizeof game_path, title_dir)) {
        snprintf(message, sizeof message, "Could not find %s in the bundle or in %s.", zw_game_rom_name, title_dir);
    } else {
        ZwParams params;
        memset(&params, 0, sizeof params);
        params.game_path = game_path;
        params.data_dir = SD_ROOT;
        zw_game_code(&params.code);
        params.new3ds = zw_game_new3ds;
        params.language = 0xFF;
        params.region = 0xFF;
        /* the main thread is on core 1, with SDL and GX2; the console goes
           where wiiu.cfg says, core 1 by default (see console_core), and the
           GPU's worker threads take the others, see zw_platform_worker_setup */
        params.emu_core = console_core;
        params.renderer = gx2 ? 1 : 0;
        if (zakuro_wiiu_start(&params) == 0) {
            started = 1;
        } else {
            snprintf(message, sizeof message, "The console could not start.");
        }
    }
    show_toast(zw_layout_name(layout));

    ZwLayoutPlan plan;
    int running = 1;
    int background = 0;
    OSTime presented = 0;
    OSTime second_start = OSGetSystemTime();
    uint32_t presents = 0;
    while (running) {
        SDL_Event event;
        while (SDL_PollEvent(&event)) {
            switch (event.type) {
            case SDL_QUIT:
                running = 0;
                break;
            case SDL_APP_WILLENTERBACKGROUND:
            case SDL_APP_DIDENTERBACKGROUND:
                background = 1;
                break;
            case SDL_APP_WILLENTERFOREGROUND:
            case SDL_APP_DIDENTERFOREGROUND:
                background = 0;
                break;
            case SDL_WINDOWEVENT:
                if (event.window.event == SDL_WINDOWEVENT_HIDDEN) background = 1;
                if (event.window.event == SDL_WINDOWEVENT_SHOWN) background = 0;
                break;
            }
        }
        if (!running) break;
        /* the HOME menu has the console: the game waits, nothing is read or
           drawn, and the heartbeat stops as a second guard */
        static int was_background;
        if (background != was_background) {
            was_background = background;
            zakuro_wiiu_pause(background);
            if (!background) reset_audio();
        }
        if (background) {
            /* GX2 is the HOME menu's now: a batch the console submitted waits */
            SDL_Delay(10);
            presented = 0;
            second_start = OSGetSystemTime();
            presents = 0;
            continue;
        }
        zakuro_wiiu_heartbeat();
        if (gx2) {
            /* until the next frame is due, run what the console asks of the
               GPU as soon as it asks, rather than once a frame */
            OSTime now = OSGetSystemTime();
            OSTime until = presented ? presented + (OSTime)OSMicrosecondsToTicks(SERVICE_US) : now;
            while ((now = OSGetSystemTime()) < until) {
                zw_gx2_service((uint32_t)OSTicksToMicroseconds(until - now));
            }
            /* one waiting batch at least, even when the frame ran late */
            zw_gx2_service(0);
        }

        zw_layout_plan(layout, tv.width, tv.height, drc.width, drc.height, &plan);
        ZwInput input;
        int cycle = 0;
        int toggle_fps = 0;
        poll_input(&input, &plan, &cycle, &toggle_fps);
        if (cycle) {
            layout = (layout + 1) % ZW_LAYOUT_COUNT;
            save_settings();
            show_toast(zw_layout_name(layout));
            zw_layout_plan(layout, tv.width, tv.height, drc.width, drc.height, &plan);
        }
        if (toggle_fps) {
            show_fps = !show_fps;
            save_settings();
            show_toast(show_fps ? "Frame rate shown" : "Frame rate hidden");
        }
        zakuro_wiiu_set_input(&input);

        if (started) {
            int32_t state = zakuro_wiiu_state();
            if (state == ZW_STATE_LOADING && !message[0]) {
                /* shown until the first frame */
            } else if (state >= ZW_STATE_EXITED && !message[0]) {
                zakuro_wiiu_message(message, sizeof message);
                if (!message[0]) snprintf(message, sizeof message, "The game stopped.");
            }
        }

        /* past the window, the console may have asked again meanwhile; a
           batch is run between each step of the main thread's own work so
           it waits for no more than one */
        if (gx2) zw_gx2_service(0);
        if (zakuro_wiiu_take_frame(top_pixels, bottom_pixels)) {
            /* each display keeps its own copy, all kept current so a layout
               switched to shows the game as it is */
            SDL_UpdateTexture(tv.top, NULL, top_pixels, ZW_TOP_WIDTH * 4);
            SDL_UpdateTexture(tv.bottom, NULL, bottom_pixels, ZW_BOTTOM_WIDTH * 4);
            SDL_UpdateTexture(drc.top, NULL, top_pixels, ZW_TOP_WIDTH * 4);
            SDL_UpdateTexture(drc.bottom, NULL, bottom_pixels, ZW_BOTTOM_WIDTH * 4);
        }
        pump_audio();

        /* while loading, what the console is doing, so a load that stalls
           says where */
        static char loading[320];
        const char *shown = message[0] ? message : NULL;
        if (!shown && started && zakuro_wiiu_state() == ZW_STATE_LOADING) {
            char status[256];
            status[0] = 0;
            zakuro_wiiu_message(status, sizeof status);
            snprintf(loading, sizeof loading, status[0] ? "Loading... %s" : "Loading...", status);
            shown = loading;
        }
        if (gx2) zw_gx2_service(0);
        draw(&drc, plan.drc_top, plan.drc_bottom, shown);
        if (gx2) zw_gx2_service(0);
        draw(&tv, plan.tv_top, plan.tv_bottom, shown);
        presented = OSGetSystemTime();
        if (toast_frames > 0) toast_frames--;
        presents++;

        /* once a second: the frame rates, and the log written out */
        if (presented - second_start >= (OSTime)OSMillisecondsToTicks(1000)) {
            update_fps(presented - second_start, presents);
            presents = 0;
            second_start = presented;
            log_flush();
        }
    }

    WHBLogPrint("closing");
    /* a batch the console waits on is dropped, so its thread can stop */
    zw_gx2_shutdown();
    if (started) zakuro_wiiu_stop();
    if (audio) SDL_CloseAudioDevice(audio);
    KPADShutdown();
    close_display(&tv);
    close_display(&drc);
    SDL_Quit();
    close_log();
    WHBLogUdpDeinit();
    WHBLogCafeDeinit();
    return 0;
}
