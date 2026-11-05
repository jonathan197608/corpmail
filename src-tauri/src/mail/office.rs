//! Office 附件预览的安全预处理与文本兜底（B8）
//!
//! 设计要点：
//! - 只支持 OOXML（docx/xlsx/pptx）与旧版 xls；doc/ppt 明确不支持；
//! - 预览前在本地做 ZIP 检查（条目数/解压总量/压缩比），阻断 ZIP 炸弹；
//! - 移除宏、OLE 嵌入对象、ActiveX、外部关系（`TargetMode="External"`），
//!   防止预览时访问外网或执行宏；
//! - 前端渲染失败时，DOCX/XLSX 可回退到纯文本（PPTX 按产品要求不降级）；
//! - 附件字节全程在内存中处理，不落临时文件、不上传。

use crate::state::{AppState, keys};
use calamine::Reader as _;
use quick_xml::Reader;
use quick_xml::Writer;
use quick_xml::events::{BytesStart, Event};
use std::fmt;
use std::io::{Cursor, Read, Write};
use zip::ZipArchive;
use zip::write::SimpleFileOptions;

/// 预览支持的 Office 类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeKind {
    Docx,
    Xlsx,
    Pptx,
    /// 旧版二进制 xls（SheetJS/calamine 可直接解析；不做 ZIP 清洗）
    Xls,
}

/// 预览错误：路由层按 `http_status` 回对应状态码，body 为可读中文原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfficeError {
    Encrypted,
    Unsupported(String),
    TooLarge(String),
    Damaged(String),
}

impl OfficeError {
    pub fn http_status(&self) -> u16 {
        match self {
            Self::TooLarge(_) => 413,
            Self::Unsupported(_) => 415,
            Self::Encrypted | Self::Damaged(_) => 422,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Encrypted => {
                "该 Office 文件已加密（受密码保护），无法直接预览；请下载后用 Office 打开。".into()
            }
            Self::Unsupported(ext) if ext.is_empty() => {
                "该格式暂不支持内嵌预览；请下载后用系统应用打开。".into()
            }
            Self::Unsupported(ext) => {
                format!("暂不支持 .{ext} 格式的内嵌预览；请下载后用系统应用打开。")
            }
            Self::TooLarge(msg) => msg.clone(),
            Self::Damaged(msg) => format!("文件损坏或结构异常，无法预览：{msg}"),
        }
    }
}

impl fmt::Display for OfficeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for OfficeError {}

/// ZIP 安全检查阈值（测试可注入更小值）
#[derive(Debug, Clone, Copy)]
pub struct ZipLimits {
    pub max_entries: usize,
    pub max_uncompressed: u64,
    pub max_ratio: u64,
}

pub const DEFAULT_ZIP_LIMITS: ZipLimits = ZipLimits {
    max_entries: 2048,
    max_uncompressed: 200 * 1024 * 1024,
    max_ratio: 200,
};

/// 文本兜底上限（字符数，非字节）
pub const MAX_TEXT_CHARS: usize = 200_000;
/// 单文件预览默认上限（MB）；可由 settings 覆盖
pub const DEFAULT_PREVIEW_MAX_MB: u64 = 20;

/// 读取预览大小上限（settings `office_preview_max_mb`，默认 20MB，钳制 1..=200）
pub fn preview_max_bytes(state: &AppState) -> u64 {
    let mb = state
        .get_or(
            keys::OFFICE_PREVIEW_MAX_MB,
            &[(keys::OFFICE_PREVIEW_MAX_MB, "20")],
        )
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_PREVIEW_MAX_MB)
        .clamp(1, 200);
    mb.saturating_mul(1024 * 1024)
}

/// 判定 Office 类型：扩展名 + 文件魔数 + ZIP 目录内容三重校验。
/// CFB 魔数且扩展名为 OOXML 时视为加密文档（OOXML 加密后是 CFB 容器）。
pub fn detect(mime: &str, filename: &str, bytes: &[u8]) -> Result<OfficeKind, OfficeError> {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = mime.to_ascii_lowercase();

    if is_zip(bytes) {
        if let Some(kind) = kind_from_zip(bytes) {
            return Ok(kind);
        }
        // 空 ZIP 或缺少 Office 目录：按扩展名报损坏，避免把任意 zip 当文档
        if matches!(
            ext.as_str(),
            "docx" | "docm" | "xlsx" | "xlsm" | "pptx" | "pptm"
        ) {
            return Err(OfficeError::Damaged("缺少 Office 文档结构".into()));
        }
        return Err(OfficeError::Unsupported(ext));
    }

    if is_cfb(bytes) {
        // 旧版 xls 是 CFB；旧版 doc/ppt 也是 CFB，但不做预览
        if ext == "xls" || mime == "application/vnd.ms-excel" {
            return Ok(OfficeKind::Xls);
        }
        if matches!(
            ext.as_str(),
            "docx" | "docm" | "xlsx" | "xlsm" | "pptx" | "pptm"
        ) || mime.contains("openxmlformats")
        {
            return Err(OfficeError::Encrypted);
        }
        return Err(OfficeError::Unsupported(ext));
    }

    if ext == "xls" || mime == "application/vnd.ms-excel" {
        // 非 CFB 的 xls（部分导出工具生成 HTML/TSV 伪 xls）交给 SheetJS 尝试
        return Ok(OfficeKind::Xls);
    }

    // 扩展名是 OOXML 但魔数不是 ZIP：损坏或实际是其他格式
    if matches!(
        ext.as_str(),
        "docx" | "docm" | "xlsx" | "xlsm" | "pptx" | "pptm"
    ) {
        return Err(OfficeError::Damaged(
            "文件头不是有效的 Office ZIP 结构".into(),
        ));
    }
    Err(OfficeError::Unsupported(ext))
}

fn is_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06")
}

fn is_cfb(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0])
}

/// 从 ZIP 目录判定 OOXML 类型（不依赖 MIME/扩展名，防伪造）
fn kind_from_zip(bytes: &[u8]) -> Option<OfficeKind> {
    let mut archive = ZipArchive::new(Cursor::new(bytes.to_vec())).ok()?;
    let mut has_word = false;
    let mut has_xl = false;
    let mut has_ppt = false;
    for i in 0..archive.len() {
        let Ok(f) = archive.by_index_raw(i) else {
            continue;
        };
        let Ok(name) = f.name() else {
            continue;
        };
        let name = name.to_ascii_lowercase();
        if name == "word/document.xml" {
            has_word = true;
        } else if name == "xl/workbook.xml" {
            has_xl = true;
        } else if name == "ppt/presentation.xml" {
            has_ppt = true;
        }
    }
    match (has_word, has_xl, has_ppt) {
        (true, _, _) => Some(OfficeKind::Docx),
        (_, true, _) => Some(OfficeKind::Xlsx),
        (_, _, true) => Some(OfficeKind::Pptx),
        _ => None,
    }
}

/// ZIP 结构检查：条目数、声明解压总量、整体压缩比
pub fn inspect_zip(bytes: &[u8], limits: ZipLimits) -> Result<(), OfficeError> {
    let mut archive = ZipArchive::new(Cursor::new(bytes.to_vec()))
        .map_err(|e| OfficeError::Damaged(format!("ZIP 解析失败：{e}")))?;
    if archive.len() > limits.max_entries {
        return Err(OfficeError::TooLarge(format!(
            "文件内部条目过多（{} > {}），已拒绝预览。",
            archive.len(),
            limits.max_entries
        )));
    }
    let mut total: u64 = 0;
    let mut packed: u64 = 0;
    for i in 0..archive.len() {
        let f = archive
            .by_index_raw(i)
            .map_err(|e| OfficeError::Damaged(format!("ZIP 目录损坏：{e}")))?;
        total = total.saturating_add(f.size());
        packed = packed.saturating_add(f.compressed_size());
        if total > limits.max_uncompressed {
            return Err(OfficeError::TooLarge(format!(
                "文件解压后体积过大（超过 {}MB），已拒绝预览。",
                limits.max_uncompressed / 1024 / 1024
            )));
        }
    }
    if packed > 0 && total / packed.max(1) > limits.max_ratio {
        return Err(OfficeError::TooLarge(
            "文件压缩比异常（疑似 ZIP 炸弹），已拒绝预览。".into(),
        ));
    }
    Ok(())
}

/// 预览用 OOXML：检查 + 清理宏/OLE/ActiveX/外部关系后重新打包。
/// XLS（CFB）原样返回，由前端 SheetJS 解析。
pub fn sanitize_for_preview(bytes: &[u8], kind: OfficeKind) -> Result<Vec<u8>, OfficeError> {
    if kind == OfficeKind::Xls {
        return Ok(bytes.to_vec());
    }
    sanitize_ooxml(bytes, DEFAULT_ZIP_LIMITS)
}

fn sanitize_ooxml(bytes: &[u8], limits: ZipLimits) -> Result<Vec<u8>, OfficeError> {
    inspect_zip(bytes, limits)?;
    let mut archive = ZipArchive::new(Cursor::new(bytes.to_vec()))
        .map_err(|e| OfficeError::Damaged(format!("ZIP 解析失败：{e}")))?;
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for i in 0..archive.len() {
        let mut f = archive
            .by_index(i)
            .map_err(|e| OfficeError::Damaged(format!("读取条目失败：{e}")))?;
        let name = f
            .name()
            .map_err(|e| OfficeError::Damaged(format!("读取条目名失败：{e}")))?
            .to_string();
        let lower = name.to_ascii_lowercase();
        if f.is_dir() || should_drop(&lower) {
            continue;
        }
        // 实际解压量校验：不能只信中央目录声明的大小
        let mut data = Vec::new();
        f.by_ref()
            .take(limits.max_uncompressed.saturating_add(1))
            .read_to_end(&mut data)
            .map_err(|e| OfficeError::Damaged(format!("解压条目失败：{e}")))?;
        if data.len() as u64 > limits.max_uncompressed {
            return Err(OfficeError::TooLarge(format!(
                "文件解压后体积过大（超过 {}MB），已拒绝预览。",
                limits.max_uncompressed / 1024 / 1024
            )));
        }
        if lower.ends_with(".rels") {
            data = strip_external_rels(&data);
        }
        writer
            .start_file(name, opts)
            .map_err(|e| OfficeError::Damaged(format!("重打包失败：{e}")))?;
        writer
            .write_all(&data)
            .map_err(|e| OfficeError::Damaged(format!("写入预览副本失败：{e}")))?;
    }
    let out = writer
        .finish()
        .map_err(|e| OfficeError::Damaged(format!("完成预览副本失败：{e}")))?;
    Ok(out.into_inner())
}

/// 宏 / OLE 嵌入 / ActiveX / 外部工作簿链接 / 自定义 UI：预览一律移除
fn should_drop(lower: &str) -> bool {
    lower.ends_with("vbaproject.bin")
        || lower.contains("/embeddings/")
        || lower.contains("/activex/")
        || lower.starts_with("customui/")
        || lower.contains("/customui/")
        || lower.starts_with("xl/externallinks/")
}

/// 删除 `.rels` 中 `TargetMode="External"` 的关系：阻止渲染时加载外网图片/
/// 打开外部链接（宏与嵌入对象已在上层整体丢弃）。
fn strip_external_rels(data: &[u8]) -> Vec<u8> {
    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::new());
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Empty(e)) if is_external_rel(&e) => {}
            Ok(Event::Start(e)) if is_external_rel(&e) => {
                // 罕见但合法：成对标签；跳过整个元素
                let target = e.name().as_ref().to_string();
                let mut depth = 1usize;
                loop {
                    match reader.read_event_into(&mut buf) {
                        Ok(Event::Start(s)) => {
                            if s.name().as_ref() == target {
                                depth += 1;
                            }
                        }
                        Ok(Event::End(en)) => {
                            if en.name().as_ref() == target {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                        }
                        Ok(Event::Eof) | Err(_) => break,
                        Ok(_) => {}
                    }
                    buf.clear();
                }
            }
            Ok(ev) => {
                if writer.write_event(ev).is_err() {
                    return empty_rels();
                }
            }
            Err(_) => return empty_rels(),
        }
        buf.clear();
    }
    let out = writer.into_inner();
    if out.is_empty() { empty_rels() } else { out }
}

fn empty_rels() -> Vec<u8> {
    br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"></Relationships>"#.to_vec()
}

fn is_external_rel(e: &BytesStart<'_>) -> bool {
    if e.name().local_name().as_ref() != "Relationship" {
        return false;
    }
    e.attributes().flatten().any(|a| {
        a.key.local_name().as_ref() == "TargetMode"
            && a.value.as_ref().eq_ignore_ascii_case("External")
    })
}

/// 文本兜底：DOCX/PPTX 抽 XML 文本；XLSX/XLS 走 calamine。
/// PPTX 按产品要求不提供文本降级（路由层会拒绝），此函数保留给测试/未来使用。
pub fn extract_text(bytes: &[u8], kind: OfficeKind) -> Result<String, OfficeError> {
    let text = match kind {
        OfficeKind::Docx => {
            let xml = read_zip_entry(bytes, "word/document.xml")
                .ok_or_else(|| OfficeError::Damaged("缺少 word/document.xml".into()))?;
            xml_text(&xml, XmlMode::Docx)
        }
        OfficeKind::Pptx => {
            let mut slides: Vec<(u32, String)> = Vec::new();
            let mut archive = ZipArchive::new(Cursor::new(bytes.to_vec()))
                .map_err(|e| OfficeError::Damaged(format!("ZIP 解析失败：{e}")))?;
            for i in 0..archive.len() {
                let Ok(mut f) = archive.by_index(i) else {
                    continue;
                };
                let Ok(name) = f.name() else {
                    continue;
                };
                let name = name.to_string();
                let Some(no) = slide_no(&name) else {
                    continue;
                };
                let mut xml = Vec::new();
                if f.read_to_end(&mut xml).is_ok() {
                    slides.push((no, xml_text(&xml, XmlMode::Pptx)));
                }
            }
            slides.sort_by_key(|(no, _)| *no);
            let mut out = String::new();
            for (no, body) in slides {
                out.push_str(&format!("【第 {no} 页】\n{body}\n"));
                if out.chars().count() >= MAX_TEXT_CHARS {
                    break;
                }
            }
            out
        }
        OfficeKind::Xlsx | OfficeKind::Xls => xls_text(bytes)?,
    };
    Ok(cap_text(text))
}

fn read_zip_entry(bytes: &[u8], name: &str) -> Option<Vec<u8>> {
    let mut archive = ZipArchive::new(Cursor::new(bytes.to_vec())).ok()?;
    let mut f = archive.by_name(name).ok()?;
    let mut out = Vec::new();
    f.read_to_end(&mut out).ok()?;
    Some(out)
}

fn slide_no(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("ppt/slides/slide")?;
    let no = rest.strip_suffix(".xml")?;
    no.parse().ok()
}

#[derive(Clone, Copy)]
enum XmlMode {
    Docx,
    Pptx,
}

/// 抽取 `<w:t>` / `<a:t>` 文本；段落换行、制表符/换行符保留
fn xml_text(data: &[u8], mode: XmlMode) -> String {
    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(false);
    let mut out = String::new();
    let mut in_text = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if e.name().local_name().as_ref() == "t" {
                    in_text = true;
                }
            }
            Ok(Event::End(e)) => match e.name().local_name().as_ref() {
                "t" => in_text = false,
                "p" => push_newline(&mut out),
                "tc" if matches!(mode, XmlMode::Docx) => out.push('\t'),
                "tr" if matches!(mode, XmlMode::Docx) => push_newline(&mut out),
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.name().local_name().as_ref() {
                "tab" if matches!(mode, XmlMode::Docx) => out.push('\t'),
                "br" | "cr" => push_newline(&mut out),
                _ => {}
            },
            Ok(Event::Text(t)) if in_text => {
                out.push_str(&t.xml10_content());
            }
            Ok(Event::CData(t)) if in_text => {
                out.push_str(t.as_ref());
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        if out.chars().count() >= MAX_TEXT_CHARS {
            break;
        }
        buf.clear();
    }
    out
}

fn push_newline(out: &mut String) {
    while out.ends_with('\n') {
        out.pop();
    }
    if !out.is_empty() {
        out.push('\n');
    }
}

fn xls_text(bytes: &[u8]) -> Result<String, OfficeError> {
    let mut wb = calamine::open_workbook_auto_from_rs(Cursor::new(bytes.to_vec()))
        .map_err(|e| OfficeError::Damaged(format!("表格解析失败：{e}")))?;
    let names = wb.sheet_names();
    let mut out = String::new();
    for name in names.iter().take(20) {
        let range = wb
            .worksheet_range(name)
            .map_err(|e| OfficeError::Damaged(format!("读取工作表失败：{e}")))?;
        out.push_str(&format!("【工作表：{name}】\n"));
        for row in range.rows().take(2000) {
            let line = row
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join("\t");
            if !line.is_empty() {
                out.push_str(line.trim_end());
                out.push('\n');
            }
            if out.chars().count() >= MAX_TEXT_CHARS {
                break;
            }
        }
        if out.chars().count() >= MAX_TEXT_CHARS {
            break;
        }
        out.push('\n');
    }
    Ok(out)
}

fn cap_text(text: String) -> String {
    if text.chars().count() <= MAX_TEXT_CHARS {
        return text;
    }
    let mut out: String = text.chars().take(MAX_TEXT_CHARS).collect();
    out.push_str("\n…（内容过长，已截断）");
    out
}

#[cfg(test)]
#[path = "../tests/office.rs"]
mod tests;
