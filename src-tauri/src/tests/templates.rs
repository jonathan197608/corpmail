//! templates 模块单元测试（自 src/templates.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;

#[test]
fn env_registers_all_templates() {
    for (name, _) in TEMPLATES {
        assert!(ENV.get_template(name).is_ok(), "模板 {name} 未注册");
    }
}

/// 文件大小自动选单位并保留 1 位小数（M9）
#[test]
fn fmt_size_picks_unit_and_one_decimal() {
    assert_eq!(fmt_size(0), "0 B");
    assert_eq!(fmt_size(1023), "1023 B");
    assert_eq!(fmt_size(1024), "1.0 KB");
    assert_eq!(fmt_size(1536), "1.5 KB");
    assert_eq!(fmt_size(5 * 1024 * 1024), "5.0 MB");
    assert_eq!(fmt_size(1024 * 1024 * 1024), "1.0 GB");
    assert_eq!(fmt_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    // 跨级 1024 进位
    assert_eq!(fmt_size(1024 * 1024), "1.0 MB");
    assert_eq!(fmt_size(1024 * 1024 * 1024 * 1024), "1.0 TB");
    assert_eq!(fmt_size(-1), "0 B");
}
