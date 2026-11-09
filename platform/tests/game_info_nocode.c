/* written by 3dsrecomp's wiiu converter for each game: what the platform
   layer and the runtime need to know of it. @...@ are filled in. */
#include "game_info.h"

const uint64_t zw_game_program_id = 0x0004000000000000ULL;
const char zw_game_title[] = "Interpreter test";
const int32_t zw_game_default_layout = 0;
const char zw_game_rom_name[] = "game.cxi";
const uint8_t zw_game_new3ds = 0;

#if 0
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
