#include "layout.h"
#include "game_info.h"

/* content of cw x ch scaled to fit inside the area, centered, aspect kept */
static ZwRect fit(int cw, int ch, int ax, int ay, int aw, int ah) {
    ZwRect r;
    /* compare cw/ch with aw/ah without floats */
    if ((long)aw * ch <= (long)ah * cw) {
        r.w = aw;
        r.h = (int)((long)aw * ch / cw);
    } else {
        r.h = ah;
        r.w = (int)((long)ah * cw / ch);
    }
    r.x = ax + (aw - r.w) / 2;
    r.y = ay + (ah - r.h) / 2;
    return r;
}

/* the two screens stacked, top over bottom and both centered, fit inside
   a display: the stack is 400 x 480 */
static void stack(int w, int h, ZwRect *top, ZwRect *bottom) {
    ZwRect all = fit(400, 480, 0, 0, w, h);
    int half = all.h / 2;
    top->w = all.w;
    top->h = half;
    top->x = all.x;
    top->y = all.y;
    bottom->w = all.w * 320 / 400;
    bottom->h = all.h - half;
    bottom->x = all.x + (all.w - bottom->w) / 2;
    bottom->y = all.y + half;
}

void zw_layout_plan(int layout, int tv_w, int tv_h, int drc_w, int drc_h, ZwLayoutPlan *out) {
    static const ZwRect none = {0, 0, 0, 0};
    out->tv_top = out->tv_bottom = out->drc_top = out->drc_bottom = none;
    out->touch_whole_panel = 0;
    switch (layout) {
    case ZW_LAYOUT_TV:
        stack(tv_w, tv_h, &out->tv_top, &out->tv_bottom);
        out->touch_whole_panel = 1;
        break;
    case ZW_LAYOUT_GAMEPAD:
        stack(drc_w, drc_h, &out->drc_top, &out->drc_bottom);
        break;
    case ZW_LAYOUT_SPLIT:
    default:
        out->tv_top = fit(400, 240, 0, 0, tv_w, tv_h);
        out->drc_bottom = fit(320, 240, 0, 0, drc_w, drc_h);
        break;
    }
}

int zw_layout_touch(const ZwLayoutPlan *plan, int drc_w, int drc_h, float px, float py, uint16_t *tx, uint16_t *ty) {
    ZwRect r = plan->drc_bottom;
    if (r.w == 0) {
        if (!plan->touch_whole_panel) return 0;
        r.x = 0;
        r.y = 0;
        r.w = drc_w;
        r.h = drc_h;
    }
    float u = (px - r.x) / (float)r.w;
    float v = (py - r.y) / (float)r.h;
    if (u < 0.0f || v < 0.0f || u >= 1.0f || v >= 1.0f) return 0;
    int x = (int)(u * 320.0f), y = (int)(v * 240.0f);
    *tx = (uint16_t)(x > 319 ? 319 : x);
    *ty = (uint16_t)(y > 239 ? 239 : y);
    return 1;
}

const char *zw_layout_name(int layout) {
    switch (layout) {
    case ZW_LAYOUT_TV: return "Both screens on the TV";
    case ZW_LAYOUT_GAMEPAD: return "Both screens on the GamePad";
    default: return "Top on the TV, bottom on the GamePad";
    }
}
