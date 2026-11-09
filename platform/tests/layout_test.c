/* host test for layout.c: cc -I.. layout_test.c ../layout.c && ./a.out */
#include <assert.h>
#include <stdio.h>
#include "../layout.h"
#include "../game_info.h"

#define EQ(r, X, Y, W, H) do { if ((r).x != (X) || (r).y != (Y) || (r).w != (W) || (r).h != (H)) { \
    printf("%s:%d %s = %d,%d %dx%d, wanted %d,%d %dx%d\n", __FILE__, __LINE__, #r, (r).x, (r).y, (r).w, (r).h, X, Y, W, H); return 1; } } while (0)

int main(void) {
    ZwLayoutPlan p;
    uint16_t x, y;

    zw_layout_plan(ZW_LAYOUT_SPLIT, 1280, 720, 854, 480, &p);
    EQ(p.tv_top, 40, 0, 1200, 720);
    EQ(p.drc_bottom, 107, 0, 640, 480);
    assert(p.tv_bottom.w == 0 && p.drc_top.w == 0);
    assert(zw_layout_touch(&p, 854, 480, 107, 0, &x, &y) && x == 0 && y == 0);
    assert(zw_layout_touch(&p, 854, 480, 746.9f, 479.9f, &x, &y) && x == 319 && y == 239);
    assert(!zw_layout_touch(&p, 854, 480, 50, 50, &x, &y));

    zw_layout_plan(ZW_LAYOUT_SPLIT, 1920, 1080, 854, 480, &p);
    EQ(p.tv_top, 60, 0, 1800, 1080);

    zw_layout_plan(ZW_LAYOUT_TV, 1280, 720, 854, 480, &p);
    EQ(p.tv_top, 340, 0, 600, 360);
    EQ(p.tv_bottom, 400, 360, 480, 360);
    assert(p.drc_top.w == 0 && p.drc_bottom.w == 0 && p.touch_whole_panel);
    assert(zw_layout_touch(&p, 854, 480, 427, 240, &x, &y) && x == 160 && y == 120);

    zw_layout_plan(ZW_LAYOUT_GAMEPAD, 1280, 720, 854, 480, &p);
    EQ(p.drc_top, 227, 0, 400, 240);
    EQ(p.drc_bottom, 267, 240, 320, 240);
    assert(p.tv_top.w == 0 && p.tv_bottom.w == 0);
    assert(zw_layout_touch(&p, 854, 480, 267, 240, &x, &y) && x == 0 && y == 0);
    assert(!zw_layout_touch(&p, 854, 480, 300, 100, &x, &y));

    puts("layout ok");
    return 0;
}
