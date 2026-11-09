/* written by 3dsrecomp's wiiu converter for each game: what the platform
   layer and the runtime need to know of it. @...@ are filled in. */
#include "game_info.h"

const uint64_t zw_game_program_id = @PROGRAM_ID@ULL;
const char zw_game_title[] = @TITLE@;
const int32_t zw_game_default_layout = @LAYOUT@;
const char zw_game_rom_name[] = @ROM_NAME@;
const uint8_t zw_game_new3ds = @NEW3DS@;
const uint8_t zw_game_renderer = @RENDERER@;

#if @HAS_CODE@
/* 3dsrecomp's tables, in librecomp.a; only their addresses matter here */
extern const uint32_t recomp_abi, recomp_generation, recomp_entry_count, recomp_module_count;
extern const char recomp_entries[], recomp_modules[], recomp_origins[];

void zw_game_code(ZwCode *code) {
    code->program_id = zw_game_program_id;
    code->abi = recomp_abi;
    code->generation = recomp_generation;
    code->entries = recomp_entries;
    code->entry_count = recomp_entry_count;
    code->modules = recomp_modules;
    code->module_count = recomp_module_count;
    code->origins = recomp_origins;
    code->present = 1;
}
#else
void zw_game_code(ZwCode *code) {
    code->program_id = zw_game_program_id;
    code->present = 0;
}
#endif
