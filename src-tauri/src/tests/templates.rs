//! templates 模块单元测试（自 src/templates.rs 内联 #[cfg(test)] mod tests 集中迁移；
//! 经 #[path] 引入回原模块，use super::* 保留白盒语义）

use super::*;

#[test]
fn env_registers_all_templates() {
    for (name, _) in TEMPLATES {
        assert!(ENV.get_template(name).is_ok(), "模板 {name} 未注册");
    }
}
