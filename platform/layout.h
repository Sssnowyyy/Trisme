/* where the console's two screens go on the TV and the GamePad, for each
   of the three layouts, and what a touch on the GamePad means. */

#ifndef ZAKURO_WIIU_LAYOUT_H
#define ZAKURO_WIIU_LAYOUT_H

#include <stdint.h>

typedef struct ZwRect {
    int x, y, w, h; /* w == 0: not shown */
} ZwRect;

typedef struct ZwLayoutPlan {
    ZwRect tv_top, tv_bottom;
    ZwRect drc_top, drc_bottom;
    /* with the bottom screen off the GamePad, its whole panel stands for
       the bottom screen, so it can still be touched without looking */
    int touch_whole_panel;
} ZwLayoutPlan;

/* layout is ZW_LAYOUT_*; sizes are the displays' in pixels */
void zw_layout_plan(int layout, int tv_w, int tv_h, int drc_w, int drc_h, ZwLayoutPlan *out);

/* a touch at (px, py) in GamePad pixels as bottom screen pixels; 0 when it
   misses the bottom screen */
int zw_layout_touch(const ZwLayoutPlan *plan, int drc_w, int drc_h, float px, float py, uint16_t *tx, uint16_t *ty);

const char *zw_layout_name(int layout);

#endif
