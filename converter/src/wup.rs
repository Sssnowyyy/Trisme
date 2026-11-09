//! the unpacked layout of an installable Wii U title, code/ content/ meta/,
//! the way NUSPacker takes it to make the WUP that WUP Installer GX2 or
//! WUPInstaller installs to the Home Menu. Packing and installing need the
//! console's common key and a homebrew environment, which are the user's.

use std::fmt::Write as _;
use std::path::Path;

use crate::images::{self, Picture};

pub struct WupInfo<'a> {
    /// the Wii U title id, 00050000 1xxxxxxx
    pub title_id: u64,
    pub rpx_name: &'a str,
    pub long_name: &'a str,
    pub short_name: &'a str,
    pub publisher: &'a str,
    pub product_code: &'a str,
    /// bytes of code the RPX loads, for cos.xml's max_codesize
    pub code_size: u64,
}

/// a Wii U title id for a 3DS one, stable from conversion to conversion:
/// 00050000 then 1 and seven hex digits from the 3DS id.
pub fn title_id_for(program_id: u64) -> u64 {
    // FNV-1a over the 3DS id
    let mut hash: u32 = 0x811C_9DC5;
    for byte in program_id.to_le_bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    0x0005_0000_0000_0000 | 0x1000_0000 | (hash as u64 & 0x0FFF_FF00)
}

fn escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '"' => "&quot;".to_owned(),
            '\'' => "&apos;".to_owned(),
            c if (c as u32) < 0x20 => " ".to_owned(),
            c => c.to_string(),
        })
        .collect()
}

fn app_xml(info: &WupInfo) -> String {
    let group = (info.title_id >> 8) & 0xFFFF;
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<app type="complex" access="777">
  <version type="unsignedInt" length="4">16</version>
  <os_version type="hexBinary" length="8">000500101000400A</os_version>
  <title_id type="hexBinary" length="8">{tid:016X}</title_id>
  <title_version type="hexBinary" length="2">0000</title_version>
  <sdk_version type="unsignedInt" length="4">21204</sdk_version>
  <app_type type="hexBinary" length="4">80000000</app_type>
  <group_id type="hexBinary" length="4">{group:08X}</group_id>
  <os_mask type="hexBinary" length="32">0000000000000000000000000000000000000000000000000000000000000000</os_mask>
  <common_id type="hexBinary" length="8">0000000000000000</common_id>
</app>
"#,
        tid = info.title_id
    )
}

fn cos_xml(info: &WupInfo) -> String {
    // the code area: what the RPX needs and a margin, in MiB steps, within
    // what the loader allows a title
    let code = (info.code_size + (8 << 20)).next_multiple_of(1 << 20).clamp(0x0080_0000, 0x0E00_0000);
    let mut permissions = String::new();
    for (index, group) in [1u32, 3, 9, 10, 11, 12, 13, 14, 15, 19, 20, 21, 22, 23].iter().enumerate() {
        let _ = writeln!(
            permissions,
            "    <p{index} type=\"complex\">\n      <group type=\"unsignedInt\" length=\"4\">{group}</group>\n      <mask type=\"hexBinary\" length=\"8\">FFFFFFFFFFFFFFFF</mask>\n    </p{index}>"
        );
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<app type="complex" access="777">
  <version type="unsignedInt" length="4">1</version>
  <cmdFlags type="unsignedInt" length="4">0</cmdFlags>
  <argstr type="string" length="4096">{rpx}</argstr>
  <avail_size type="hexBinary" length="4">00000000</avail_size>
  <codegen_size type="hexBinary" length="4">00000000</codegen_size>
  <codegen_core type="hexBinary" length="4">80000001</codegen_core>
  <max_size type="hexBinary" length="4">40000000</max_size>
  <max_codesize type="hexBinary" length="4">{code:08X}</max_codesize>
  <overlay_arena type="hexBinary" length="4">00000000</overlay_arena>
  <default_stack0_size type="hexBinary" length="4">00000000</default_stack0_size>
  <default_stack1_size type="hexBinary" length="4">00000000</default_stack1_size>
  <default_stack2_size type="hexBinary" length="4">00000000</default_stack2_size>
  <default_redzone0_size type="hexBinary" length="4">00000000</default_redzone0_size>
  <default_redzone1_size type="hexBinary" length="4">00000000</default_redzone1_size>
  <default_redzone2_size type="hexBinary" length="4">00000000</default_redzone2_size>
  <exception_stack0_size type="hexBinary" length="4">00001000</exception_stack0_size>
  <exception_stack1_size type="hexBinary" length="4">00001000</exception_stack1_size>
  <exception_stack2_size type="hexBinary" length="4">00001000</exception_stack2_size>
  <permissions type="complex">
{permissions}  </permissions>
</app>
"#,
        rpx = escape(info.rpx_name)
    )
}

fn meta_xml(info: &WupInfo) -> String {
    let tid = format!("{:016X}", info.title_id);
    let mut names = String::new();
    for language in ["ja", "en", "fr", "de", "it", "es", "zhs", "ko", "nl", "pt", "ru", "zht"] {
        let _ = writeln!(names, "  <longname_{language} type=\"string\" length=\"512\">{}</longname_{language}>", escape(info.long_name));
        let _ = writeln!(names, "  <shortname_{language} type=\"string\" length=\"256\">{}</shortname_{language}>", escape(info.short_name));
        let _ = writeln!(names, "  <publisher_{language} type=\"string\" length=\"256\">{}</publisher_{language}>", escape(info.publisher));
    }
    // a product code of the Wii U's shape, from the 3DS one's last letters
    let code: String = info.product_code.chars().filter(char::is_ascii_alphanumeric).collect();
    let tail = if code.len() >= 4 { &code[code.len() - 4..] } else { "ZKRO" };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<menu type="complex" access="777">
  <version type="unsignedInt" length="4">33</version>
  <product_code type="string" length="32">WUP-N-{tail}</product_code>
  <content_platform type="string" length="32">WUP</content_platform>
  <company_code type="string" length="8">0001</company_code>
  <mastering_date type="string" length="32"></mastering_date>
  <logo_type type="unsignedInt" length="4">0</logo_type>
  <app_launch_type type="hexBinary" length="4">00000000</app_launch_type>
  <invisible_flag type="hexBinary" length="4">00000000</invisible_flag>
  <no_managed_flag type="hexBinary" length="4">00000000</no_managed_flag>
  <no_event_log type="hexBinary" length="4">00000002</no_event_log>
  <no_icon_database type="hexBinary" length="4">00000000</no_icon_database>
  <launching_flag type="hexBinary" length="4">00000004</launching_flag>
  <install_flag type="hexBinary" length="4">00000000</install_flag>
  <closing_msg type="unsignedInt" length="4">0</closing_msg>
  <title_version type="unsignedInt" length="4">0</title_version>
  <title_id type="hexBinary" length="8">{tid}</title_id>
  <group_id type="hexBinary" length="4">{group:08X}</group_id>
  <boss_id type="hexBinary" length="8">0000000000000000</boss_id>
  <os_version type="hexBinary" length="8">000500101000400A</os_version>
  <app_size type="hexBinary" length="8">0000000000000000</app_size>
  <common_save_size type="hexBinary" length="8">0000000000000000</common_save_size>
  <account_save_size type="hexBinary" length="8">0000000000000000</account_save_size>
  <common_boss_size type="hexBinary" length="8">0000000000000000</common_boss_size>
  <account_boss_size type="hexBinary" length="8">0000000000000000</account_boss_size>
  <save_no_rollback type="unsignedInt" length="4">0</save_no_rollback>
  <join_game_id type="hexBinary" length="4">00000000</join_game_id>
  <join_game_mode_mask type="hexBinary" length="8">0000000000000000</join_game_mode_mask>
  <bg_daemon_enable type="unsignedInt" length="4">0</bg_daemon_enable>
  <olv_accesskey type="unsignedInt" length="4">0</olv_accesskey>
  <wood_tin type="unsignedInt" length="4">0</wood_tin>
  <e_manual type="unsignedInt" length="4">0</e_manual>
  <e_manual_version type="unsignedInt" length="4">0</e_manual_version>
  <region type="hexBinary" length="4">FFFFFFFF</region>
  <pc_cero type="unsignedInt" length="4">128</pc_cero>
  <pc_esrb type="unsignedInt" length="4">128</pc_esrb>
  <pc_bbfc type="unsignedInt" length="4">192</pc_bbfc>
  <pc_usk type="unsignedInt" length="4">128</pc_usk>
  <pc_pegi_gen type="unsignedInt" length="4">128</pc_pegi_gen>
  <pc_pegi_fin type="unsignedInt" length="4">192</pc_pegi_fin>
  <pc_pegi_prt type="unsignedInt" length="4">128</pc_pegi_prt>
  <pc_pegi_bbfc type="unsignedInt" length="4">128</pc_pegi_bbfc>
  <pc_cob type="unsignedInt" length="4">128</pc_cob>
  <pc_grb type="unsignedInt" length="4">128</pc_grb>
  <pc_cgsrr type="unsignedInt" length="4">128</pc_cgsrr>
  <pc_oflc type="unsignedInt" length="4">128</pc_oflc>
  <pc_reserved0 type="unsignedInt" length="4">192</pc_reserved0>
  <pc_reserved1 type="unsignedInt" length="4">192</pc_reserved1>
  <pc_reserved2 type="unsignedInt" length="4">192</pc_reserved2>
  <pc_reserved3 type="unsignedInt" length="4">192</pc_reserved3>
  <ext_dev_nunchaku type="unsignedInt" length="4">0</ext_dev_nunchaku>
  <ext_dev_classic type="unsignedInt" length="4">1</ext_dev_classic>
  <ext_dev_urcc type="unsignedInt" length="4">1</ext_dev_urcc>
  <ext_dev_board type="unsignedInt" length="4">0</ext_dev_board>
  <ext_dev_usb_keyboard type="unsignedInt" length="4">0</ext_dev_usb_keyboard>
  <ext_dev_etc type="unsignedInt" length="4">0</ext_dev_etc>
  <ext_dev_etc_name type="string" length="512"></ext_dev_etc_name>
  <eula_version type="unsignedInt" length="4">0</eula_version>
  <drc_use type="unsignedInt" length="4">1</drc_use>
  <network_use type="unsignedInt" length="4">0</network_use>
  <online_account_use type="unsignedInt" length="4">0</online_account_use>
  <direct_boot type="unsignedInt" length="4">0</direct_boot>
  <reserved_flag0 type="hexBinary" length="4">00000000</reserved_flag0>
  <reserved_flag1 type="hexBinary" length="4">00000000</reserved_flag1>
  <reserved_flag2 type="hexBinary" length="4">00000000</reserved_flag2>
  <reserved_flag3 type="hexBinary" length="4">00000000</reserved_flag3>
  <reserved_flag4 type="hexBinary" length="4">00000000</reserved_flag4>
  <reserved_flag5 type="hexBinary" length="4">00000000</reserved_flag5>
  <reserved_flag6 type="hexBinary" length="4">00000000</reserved_flag6>
  <reserved_flag7 type="hexBinary" length="4">00000000</reserved_flag7>
{names}  <add_on_unique_id0 type="hexBinary" length="4">00000000</add_on_unique_id0>
</menu>
"#,
        group = (info.title_id >> 8) & 0xFFFF
    )
}

/// writes code/, meta/ and the given content directory's files at dir. the
/// RPX and content are put in place by the caller.
pub fn write_layout(dir: &Path, info: &WupInfo, icon: Option<&Picture>) -> Result<(), String> {
    let write = |path: &Path, text: &str| std::fs::write(path, text).map_err(|e| format!("could not write {}, {e}", path.display()));
    let code = dir.join("code");
    let meta = dir.join("meta");
    for folder in [&code, &meta, &dir.join("content")] {
        std::fs::create_dir_all(folder).map_err(|e| format!("could not create {}, {e}", folder.display()))?;
    }
    write(&code.join("app.xml"), &app_xml(info))?;
    write(&code.join("cos.xml"), &cos_xml(info))?;
    write(&meta.join("meta.xml"), &meta_xml(info))?;
    images::menu_icon(icon).write_tga(&meta.join("iconTex.tga"), true)?;
    images::splash(icon, 1280, 720).write_tga(&meta.join("bootTvTex.tga"), false)?;
    images::splash(icon, 854, 480).write_tga(&meta.join("bootDrcTex.tga"), false)?;
    Picture::filled(170, 42, [0, 0, 0, 0]).write_tga(&meta.join("bootLogoTex.tga"), true)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_ids_have_the_homebrew_shape() {
        let id = title_id_for(0x0004_0000_000E_C300);
        assert_eq!(id >> 32, 0x0005_0000);
        assert_eq!((id >> 28) & 0xF, 1);
        assert_eq!(id & 0xFF, 0);
        assert_eq!(id, title_id_for(0x0004_0000_000E_C300));
        assert_ne!(id, title_id_for(0x0004_0000_0005_5D00));
    }

    #[test]
    fn xml_is_escaped_and_carries_the_ids() {
        let info = WupInfo {
            title_id: 0x0005_0000_1234_5600,
            rpx_name: "game.rpx",
            long_name: "A <Game> & more",
            short_name: "Game",
            publisher: "Maker",
            product_code: "CTR-P-BZLE",
            code_size: 40 << 20,
        };
        let meta = meta_xml(&info);
        assert!(meta.contains("A &lt;Game&gt; &amp; more"));
        assert!(meta.contains("0005000012345600"));
        assert!(meta.contains("WUP-N-BZLE"));
        let cos = cos_xml(&info);
        assert!(cos.contains("<argstr type=\"string\" length=\"4096\">game.rpx</argstr>"));
        assert!(cos.contains("<max_codesize type=\"hexBinary\" length=\"4\">03000000</max_codesize>"));
        assert!(app_xml(&info).contains("<group_id type=\"hexBinary\" length=\"4\">00003456</group_id>"));
    }
}
