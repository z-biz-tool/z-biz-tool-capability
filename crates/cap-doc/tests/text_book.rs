//! TXT 小说的分章判定与切分。
//!
//! 这个判定的失败方式很讨厌：判宽了，「第二天早上他醒了」会被拆成新章节，
//! 整本书碎成上千节；判窄了，一本三百章的书只有一节，翻页器一页一页挪到天荒地老。
//! 所以两边都要有反例钉住。

use cap_doc::{is_chapter_heading, split_txt_sections, txt_body_to_html};

#[test]
fn recognises_chinese_chapter_headings() {
    for case in [
        "第一章 风起",
        "第1章 风起",
        "第十二回 大闹天宫",
        "第三卷 山河",
        "第二部 归途",
        "第五集",
        "第一卷 开端",
        "楔子",
        "序章 血夜",
        "番外 一",
        "尾声",
        "Chapter 3",
        "CHAPTER XII",
        "chapter: 7",
    ] {
        assert!(is_chapter_heading(case), "应该认成章节标题: {}", case);
    }
}

#[test]
fn rejects_ordinary_sentences() {
    for case in [
        // 「第二天」在正文里太常见，绝不能当标题
        "第二天早上他醒了",
        "第三次世界大战爆发了",
        "他说第一句话的时候就后悔了",
        // 超过 40 字的整段，即使是「第X章」开头也不是标题
        "第一章讲的是一个少年在山里长大后来下山闯荡江湖经历了无数风波最终成为一代宗师的故事他知道这一切都要从那个雨夜说起",
        // 英文句子
        "Chapter by chapter we walked through the field",
        "",
        "   ",
    ] {
        assert!(!is_chapter_heading(case), "不该认成章节标题: {}", case);
    }
}

#[test]
fn splits_book_into_sections_with_preface() {
    let text = "书名：测试\n作者：某人\n\n第一章 开端\n正文一\n正文二\n\n第二章 转折\n正文三\n";
    let sections = split_txt_sections(text);
    assert_eq!(sections.len(), 3, "前言 + 两章");
    assert_eq!(sections[0].title, "正文");
    assert!(sections[0].body.contains("书名"));
    assert_eq!(sections[1].title, "第一章 开端");
    assert!(sections[1].body.contains("正文二"));
    assert_eq!(sections[2].title, "第二章 转折");
}

#[test]
fn book_without_headings_stays_one_section() {
    let text = "只有一段文字。\n又一段。\n";
    let sections = split_txt_sections(text);
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].title, "正文");
}

#[test]
fn chapter_without_body_is_dropped() {
    // 末章只有标题、没有正文（有的 TXT 结尾就是这样），不该产出一个空章节
    let text = "第一章 A\n正文一\n第二章 B\n";
    let sections = split_txt_sections(text);
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].title, "第一章 A");
    assert!(sections[0].body.contains("正文一"));
}

#[test]
fn body_becomes_one_paragraph_per_line() {
    // 中文 TXT 是「一行一段」，如果按空行分段，整本书会变成几十个巨段
    let html = txt_body_to_html("第一行\n第二行\n\n第三行");
    assert_eq!(html, "<p>第一行</p><p>第二行</p><p>第三行</p>");
}

#[test]
fn html_special_chars_in_body_are_escaped() {
    let html = txt_body_to_html("<b>不是标签</b> & 100%");
    assert!(html.contains("&lt;b&gt;"));
    assert!(html.contains("&amp;"));
    // 真的标签不能漏出去
    assert!(!html.contains("<b>"));
}
