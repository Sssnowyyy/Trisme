/* what the converter writes for each game, in game_info.c, compiled into
   the program next to the game's recompiled code. */

#ifndef ZAKURO_WIIU_GAME_INFO_H
#define ZAKURO_WIIU_GAME_INFO_H

#include <stdint.h>
#include "zakuro_wiiu.h"

/* the screen layouts, see layout.c */
enum {
    ZW_LAYOUT_SPLIT = 0,   /* top screen on the TV, bottom on the GamePad */
    ZW_LAYOUT_TV = 1,      /* both screens on the TV */
    ZW_LAYOUT_GAMEPAD = 2, /* both screens on the GamePad */
    ZW_LAYOUT_COUNT = 3,
};

extern const uint64_t zw_game_program_id;
extern const char zw_game_title[];
/* the layout a first start uses, before the player picks another */
extern const int32_t zw_game_default_layout;
/* the file name of the game in the bundle's content, e.g. "game.cxi" */
extern const char zw_game_rom_name[];
extern const uint8_t zw_game_new3ds;
/* where the 3D is drawn until wiiu.cfg says otherwise: 1 the GPU (GX2),
   0 the CPU */
extern const uint8_t zw_game_renderer;

/* fills code with the recomp_ symbols of the code linked in, or marks it
   absent for an interpreter-only build. */
void zw_game_code(ZwCode *code);

#endif
