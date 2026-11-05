//! Office 预览后端测试：格式判定、ZIP 安全、外部关系清理、文本兜底

use super::*;
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/office")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("读取样例 {} 失败：{e}", path.display()))
}

fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in entries {
        writer.start_file(*name, opts).unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn minimal_docx() -> Vec<u8> {
    let xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
<w:p><w:r><w:t>你好，</w:t></w:r><w:r><w:t>CorpMail</w:t></w:r></w:p>
<w:p><w:r><w:t>第二段</w:t></w:r><w:r><w:tab/><w:t>制表</w:t></w:r></w:p>
</w:body></w:document>"#;
    zip_of(&[
        ("[Content_Types].xml", b"<Types/>"),
        ("word/document.xml", xml.as_bytes()),
    ])
}

#[test]
fn detect_prefers_zip_contents_over_filename() {
    let docx = fixture("report.docx");
    assert_eq!(
        detect("application/octet-stream", "report.docx", &docx).unwrap(),
        OfficeKind::Docx
    );
    assert_eq!(
        detect("application/octet-stream", "report.bin", &docx).unwrap(),
        OfficeKind::Docx,
        "扩展名缺失时应按 ZIP 目录内容识别"
    );
    let xlsx = fixture("summary.xlsx");
    assert_eq!(
        detect("application/octet-stream", "summary.xlsx", &xlsx).unwrap(),
        OfficeKind::Xlsx
    );
    let pptx = fixture("deck.pptx");
    assert_eq!(
        detect("application/octet-stream", "deck.ppt", &pptx).unwrap(),
        OfficeKind::Pptx
    );
}

#[test]
fn detect_flags_encrypted_and_legacy_binary() {
    let cfb = [0xD0, 0xCF, 0x11, 0xE0, 0, 0, 0, 0];
    assert_eq!(
        detect("", "secret.docx", &cfb),
        Err(OfficeError::Encrypted),
        "OOXML 扩展名 + CFB 魔数 = 加密文档"
    );
    assert_eq!(
        detect("application/vnd.ms-excel", "old.xls", &cfb).unwrap(),
        OfficeKind::Xls
    );
    let xls = fixture("summary.xls");
    assert_eq!(
        detect("application/vnd.ms-excel", "summary.xls", &xls).unwrap(),
        OfficeKind::Xls
    );
    assert_eq!(
        sanitize_for_preview(&xls, OfficeKind::Xls).unwrap(),
        xls,
        "旧版 xls 不做 ZIP 清洗，原样返回"
    );
    assert!(matches!(
        detect("", "old.doc", &cfb),
        Err(OfficeError::Unsupported(_))
    ));
    assert!(matches!(
        detect("", "old.ppt", &cfb),
        Err(OfficeError::Unsupported(_))
    ));
}

#[test]
fn inspect_zip_rejects_entry_count_and_uncompressed_bomb() {
    let many = zip_of(&[("a.xml", b"x"), ("b.xml", b"x"), ("c.xml", b"x")]);
    let limits = ZipLimits {
        max_entries: 2,
        max_uncompressed: 1024,
        max_ratio: 200,
    };
    assert!(matches!(
        inspect_zip(&many, limits),
        Err(OfficeError::TooLarge(_))
    ));

    let big = zip_of(&[("word/document.xml", &[b'a'; 4096])]);
    let limits = ZipLimits {
        max_entries: 10,
        max_uncompressed: 1024,
        max_ratio: 200,
    };
    assert!(matches!(
        inspect_zip(&big, limits),
        Err(OfficeError::TooLarge(_))
    ));
}

#[test]
fn sanitize_removes_macros_embeddings_and_external_relationships() {
    let rels = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://evil.example/x" TargetMode="External"/>
<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="mailto:a@b.c" TargetMode="External"/>
</Relationships>"#;
    let raw = zip_of(&[
        ("[Content_Types].xml", b"<Types/>"),
        ("word/document.xml", b"<w:document/>"),
        ("word/vbaProject.bin", b"macro"),
        ("word/embeddings/oleObject1.bin", b"ole"),
        ("word/_rels/document.xml.rels", rels),
    ]);
    let clean = sanitize_for_preview(&raw, OfficeKind::Docx).unwrap();
    let mut archive = ZipArchive::new(Cursor::new(clean)).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index_raw(i).unwrap().name().unwrap().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "word/document.xml"));
    assert!(
        !names.iter().any(|n| n.contains("vbaProject")),
        "宏应移除：{names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("embeddings")),
        "OLE 嵌入应移除：{names:?}"
    );
    let rels_out = {
        let mut f = archive.by_name("word/_rels/document.xml.rels").unwrap();
        let mut out = String::new();
        f.read_to_string(&mut out).unwrap();
        out
    };
    assert!(rels_out.contains("rId1"), "内部图片关系应保留：{rels_out}");
    assert!(
        !rels_out.contains("evil.example"),
        "外部链接应移除：{rels_out}"
    );
    assert!(
        !rels_out.contains("mailto:a@b.c"),
        "外部邮件链接应移除：{rels_out}"
    );
    assert!(!rels_out.contains("TargetMode=\"External\""));
}

#[test]
fn extract_text_docx_keeps_paragraphs_and_tabs() {
    let text = extract_text(&minimal_docx(), OfficeKind::Docx).unwrap();
    assert!(text.contains("你好，CorpMail"), "{text}");
    assert!(text.contains("第二段"), "{text}");
    assert!(text.contains('\t'), "制表符应保留：{text:?}");
}

#[test]
fn extract_text_xlsx_reads_sheets_and_values() {
    let bytes = fixture("summary.xlsx");
    let text = extract_text(&bytes, OfficeKind::Xlsx).unwrap();
    assert!(text.contains("【工作表：汇总】"), "{text}");
    assert!(text.contains("宽带提速配案汇总"), "{text}");
    // 文本兜底不读取单元格样式：完成率以原始值 0.931 呈现（视觉预览由前端负责）
    assert!(text.contains("0.931"), "{text}");
    assert!(text.contains("【工作表：明细】"), "{text}");
}

#[test]
fn extract_text_xls_legacy_reads_values() {
    let bytes = fixture("summary.xls");
    let text = extract_text(&bytes, OfficeKind::Xls).unwrap();
    assert!(text.contains("【工作表：汇总】"), "{text}");
    assert!(text.contains("宽带提速配案汇总"), "{text}");
}

#[test]
fn extract_text_pptx_marks_slides() {
    let bytes = fixture("deck.pptx");
    let text = extract_text(&bytes, OfficeKind::Pptx).unwrap();
    assert!(text.contains("【第 1 页】"), "{text}");
    assert!(text.contains("宽带提速配案季度汇报"), "{text}");
    assert!(text.contains("【第 2 页】"), "{text}");
    assert!(text.contains("89.7%"), "{text}");
}

#[test]
fn preview_limit_reads_setting_and_clamps() {
    let dir = std::env::temp_dir().join(format!("corpmail-office-limit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = AppState::open(&dir);
    assert_eq!(preview_max_bytes(&state), 20 * 1024 * 1024);
    state
        .set_batch(&[(keys::OFFICE_PREVIEW_MAX_MB, "50")])
        .unwrap();
    assert_eq!(preview_max_bytes(&state), 50 * 1024 * 1024);
    state
        .set_batch(&[(keys::OFFICE_PREVIEW_MAX_MB, "0")])
        .unwrap();
    assert_eq!(preview_max_bytes(&state), 1024 * 1024, "下限 1MB");
    state
        .set_batch(&[(keys::OFFICE_PREVIEW_MAX_MB, "999")])
        .unwrap();
    assert_eq!(preview_max_bytes(&state), 200 * 1024 * 1024, "上限 200MB");
    let _ = std::fs::remove_dir_all(&dir);
}
