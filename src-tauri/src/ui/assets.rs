//! 静态资源读取：从 Tauri 编译期内嵌的 frontendDist（`ui-dist/`）取字节
//!
//! `asset_resolver` 与官方资产协议同源（EmbeddedAssets，已解压），
//! MIME 由 tauri 按资源推断；抽象出 trait 便于单测注入。

use tauri::Runtime;

/// 静态资源读取抽象，返回 `(MIME, 字节)`
pub trait Assets {
    /// 读取 dist 根下的相对路径（不带前导 `/`）
    fn get(&self, path: &str) -> Option<(String, Vec<u8>)>;
}

/// 基于 Tauri 内嵌资源的实现（debug/release 均为编译期内嵌，项目未启用 devUrl）
pub struct EmbeddedAssets<R: Runtime> {
    resolver: tauri::AssetResolver<R>,
}

impl<R: Runtime> EmbeddedAssets<R> {
    pub fn new(app: &tauri::AppHandle<R>) -> Self {
        Self {
            resolver: app.asset_resolver(),
        }
    }
}

impl<R: Runtime> Assets for EmbeddedAssets<R> {
    fn get(&self, path: &str) -> Option<(String, Vec<u8>)> {
        let asset = self.resolver.get(path.to_string())?;
        Some((asset.mime_type, asset.bytes))
    }
}
