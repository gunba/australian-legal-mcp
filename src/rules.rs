//! Source-derived metadata rules engine: classifies each doc by shape
//! (Taxation Ruling, Court Case, Act section, EM, ...) and runs a
//! template-specific extractor for (title, date). Port of the Python
//! src/ato_mcp/indexer/rules.py.


// =====================================================================
// Rules engine — template-based metadata classifier
//
// Port of src/ato_mcp/indexer/rules.py (deleted in v0.8.0). Classifies
// each doc into one of ~10 structural templates (Taxation Ruling, Court
// Case, Act, EM, ...) and runs a positional extractor for each. Output
// is a (title, date) pair the build pipeline writes into the documents
// row.
// =====================================================================

#[derive(Debug, Clone, Default)]
pub(crate) struct RuleInputs {
    pub doc_id: String,
    pub title: Option<String>,
    pub headings: Vec<String>,
    pub heading_levels: Vec<u32>,
    pub body_head: String,
    pub category: Option<String>,
    pub pub_date: Option<String>,
    pub front_matter_refs: Vec<String>,
    pub front_matter_phrase: Option<String>,
}

impl RuleInputs {
    fn outer_prefix(&self) -> String {
        self.doc_id
            .split('/')
            .find(|s| !s.is_empty())
            .map(|s| s.to_uppercase())
            .unwrap_or_default()
    }

    fn inner_body(&self) -> String {
        let segs: Vec<&str> = self.doc_id.split('/').filter(|s| !s.is_empty()).collect();
        if segs.len() >= 2 {
            segs[1].to_string()
        } else {
            String::new()
        }
    }

    fn h1(&self) -> String {
        self.headings
            .first()
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    fn h2(&self) -> String {
        self.headings
            .get(1)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DerivedMetadata {
    pub title: Option<String>,
    pub date: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Shape {
    Empty,
    RulingTypePhrase,
    GuidelineTypePhrase,
    AlertPhrase,
    AtoidPhrase,
    PslaPhrase,
    SmsfrbPhrase,
    DisPhrase,
    EmPhrase,
    RulingCitation,
    RulingUnslashed,
    Atoid,
    Psla,
    Smsfrb,
    NeutralCitation,
    NameVName,
    ReX,
    CaseNumber,
    ActTitle,
    BillTitle,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Template {
    OfficialPub,
    CaseH1,
    CaseH2,
    HistCase,
    Dis,
    Act,
    LegislationSection,
    BillEm,
    Smsfrb,
    Epa,
    Other,
}

pub(crate) const RULING_SERIES_LIST: &[&str] = &[
    // Sorted by length desc so longer prefixes match first in the alternation.
    "SMSFRB", "SMSFR", "SMSFD", "GSTR", "GSTD", "FBTR", "WETR", "WETD", "LCR", "SGR", "FTR", "PCG",
    "LCG", "PRR", "CLR", "COG", "TXD", "TPA", "FBT", "GII", "CR", "PR", "TR", "TD", "MT", "TA",
    "LI", "LG", "WT", "IT",
];

pub(crate) fn ruling_series_alt() -> &'static str {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| RULING_SERIES_LIST.join("|"))
}

pub(crate) const UNSLASHED_LEGACY_LIST: &[&str] = &["IT", "MT", "CRP"];

pub(crate) fn re_ruling_citation() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(&format!(
            r"^({})\s+\d{{1,4}}/D?\d+(?:[A-Z0-9]+)?(?:\s|$|\()",
            ruling_series_alt()
        ))
        .unwrap()
    })
}

pub(crate) fn re_ruling_unslashed() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(&format!(
            r"^({})\s+\d{{1,5}}(?:\s|$|[—\-])",
            UNSLASHED_LEGACY_LIST.join("|")
        ))
        .unwrap()
    })
}

pub(crate) fn re_atoid() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^ATO\s+ID\s+\d{4}/\d+").unwrap())
}

pub(crate) fn re_psla() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^PS\s+LA\s+\d{4}/").unwrap())
}

pub(crate) fn re_smsfrb() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^SMSFRB\s+\d{4}/").unwrap())
}

pub(crate) fn re_neutral() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^\[\d{4}\]\s+[A-Z]+\s+\d+").unwrap())
}

pub(crate) fn re_name_v_name() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^[A-Z][\w'.&\-]*(?:\s+(?:\([^)]+\)|[A-Z][\w'.&\-]*|and|of|the|for|on|in|an|Anor|ors?|No|nee))*(?:,?\s+(?:Pty\s+)?(?:Ltd|Limited|Inc\.?|LLC|Corp|Co\.?|Plc))?\s+(?:v\.?|vs\.?)\s+(?:the|a|an)?\s*(?:\([^)]+\)\s*)?[A-Za-z][\w'.&\-]*",
        )
        .unwrap()
    })
}

pub(crate) fn re_re_x() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"(?i)^(?:Re|In\s+re|In\s+the\s+Matter\s+of|Ex\s+parte)\s+[A-Z]").unwrap()
    })
}

pub(crate) fn re_case_number() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"(?i)^Case\s+[A-Z]?\d+(?:/\d+)?$").unwrap())
}

pub(crate) fn re_act_title() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^(?:[A-Za-z][\w'&\-]*|\([^)]+\))(?:\s+(?:[A-Za-z][\w'&\-]*|\([^)]+\)))*\s+(?:Act|Regulations?|Code|Rules)\s+(?:19|20)\d{2}(?:\s*\(Cth\))?\s*$",
        )
        .unwrap()
    })
}

pub(crate) fn re_bill_title() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"\bBill\s+(?:19|20)\d{2}\b").unwrap())
}

pub(crate) fn type_phrases(
) -> &'static std::collections::HashMap<Shape, std::collections::HashSet<&'static str>> {
    static M: std::sync::OnceLock<
        std::collections::HashMap<Shape, std::collections::HashSet<&'static str>>,
    > = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let mut m = std::collections::HashMap::new();
        m.insert(
            Shape::RulingTypePhrase,
            [
                "taxation ruling",
                "class ruling",
                "product ruling",
                "law companion ruling",
                "gst ruling",
                "gst determination",
                "taxation determination",
                "superannuation guarantee ruling",
                "fuel tax ruling",
                "fringe benefits tax ruling",
                "income tax ruling",
                "miscellaneous taxation ruling",
                "law companion guideline",
                "wine equalisation tax ruling",
                "wine equalisation tax determination",
                "superannuation guarantee determination",
                "smsf ruling",
                "smsf determination",
                "ruling compendium",
                "goods and services tax ruling",
                "goods and services tax determination",
            ]
            .iter()
            .copied()
            .collect(),
        );
        m.insert(
            Shape::GuidelineTypePhrase,
            [
                "practical compliance guideline",
                "practical compliance guidelines",
            ]
            .iter()
            .copied()
            .collect(),
        );
        m.insert(
            Shape::AlertPhrase,
            ["taxpayer alert"].iter().copied().collect(),
        );
        m.insert(
            Shape::AtoidPhrase,
            ["ato interpretative decision"].iter().copied().collect(),
        );
        m.insert(
            Shape::PslaPhrase,
            [
                "practice statement law administration",
                "ato practice statement law administration",
                "law administration practice statement",
            ]
            .iter()
            .copied()
            .collect(),
        );
        m.insert(
            Shape::SmsfrbPhrase,
            ["smsf regulator's bulletin", "smsf regulators bulletin"]
                .iter()
                .copied()
                .collect(),
        );
        m.insert(
            Shape::DisPhrase,
            ["decision impact statement", "decision impact statements"]
                .iter()
                .copied()
                .collect(),
        );
        m.insert(
            Shape::EmPhrase,
            [
                "explanatory memorandum",
                "supplementary explanatory memorandum",
            ]
            .iter()
            .copied()
            .collect(),
        );
        m
    })
}

pub(crate) fn shape_of(heading: &str) -> Shape {
    let t = heading.split_whitespace().collect::<Vec<&str>>().join(" ");
    if t.is_empty() {
        return Shape::Empty;
    }
    let t_lower = t.to_lowercase();
    if re_neutral().is_match(&t) {
        return Shape::NeutralCitation;
    }
    if re_atoid().is_match(&t) {
        return Shape::Atoid;
    }
    if re_psla().is_match(&t) {
        return Shape::Psla;
    }
    if re_smsfrb().is_match(&t) {
        return Shape::Smsfrb;
    }
    if re_ruling_citation().is_match(&t) {
        return Shape::RulingCitation;
    }
    if re_ruling_unslashed().is_match(&t) {
        return Shape::RulingUnslashed;
    }
    for (sh, phrases) in type_phrases().iter() {
        if phrases.contains(t_lower.as_str()) {
            return *sh;
        }
    }
    if re_act_title().is_match(&t) {
        return Shape::ActTitle;
    }
    if re_bill_title().is_match(&t) {
        return Shape::BillTitle;
    }
    if re_re_x().is_match(&t) {
        return Shape::ReX;
    }
    if re_case_number().is_match(&t) {
        return Shape::CaseNumber;
    }
    if re_name_v_name().is_match(&t) && t.len() < 200 {
        return Shape::NameVName;
    }
    Shape::Other
}

pub(crate) fn re_docid_jud_star() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^\*(\d{4})\*(.+)$").unwrap())
}

pub(crate) fn re_docid_act_section() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^(\d{4})(\d{4})$").unwrap())
}

pub(crate) fn classify(ins: &RuleInputs) -> Template {
    let shapes: Vec<Shape> = ins.headings.iter().take(6).map(|h| shape_of(h)).collect();
    let has = |s: Shape| shapes.contains(&s);
    let any_citation = shapes.iter().any(|s| {
        matches!(
            s,
            Shape::RulingCitation | Shape::RulingUnslashed | Shape::Atoid | Shape::Psla
        )
    });

    if has(Shape::Smsfrb) || has(Shape::SmsfrbPhrase) {
        return Template::Smsfrb;
    }
    let inner = ins.inner_body();
    let outer = ins.outer_prefix();
    if outer == "JUD" && re_docid_jud_star().is_match(&inner) {
        return Template::HistCase;
    }
    if (outer == "PAC" || outer == "REG") && re_docid_act_section().is_match(&inner) {
        return Template::LegislationSection;
    }
    if any_citation {
        return Template::OfficialPub;
    }
    if has(Shape::DisPhrase)
        && shapes
            .iter()
            .any(|s| matches!(s, Shape::NameVName | Shape::ReX | Shape::NeutralCitation))
    {
        return Template::Dis;
    }
    if !shapes.is_empty()
        && matches!(
            shapes[0],
            Shape::NameVName | Shape::ReX | Shape::NeutralCitation | Shape::CaseNumber
        )
    {
        return Template::CaseH1;
    }
    if shapes.len() >= 2
        && shapes[1] == Shape::NameVName
        && ins.category.as_deref() == Some("Cases")
    {
        return Template::CaseH2;
    }
    if ins.category.as_deref() == Some("Cases") {
        if shapes.iter().any(|s| {
            matches!(
                s,
                Shape::NameVName | Shape::ReX | Shape::NeutralCitation | Shape::CaseNumber
            )
        }) {
            return Template::CaseH1;
        }
        return Template::CaseH1;
    }
    if !shapes.is_empty() && shapes[0] == Shape::ActTitle {
        return Template::Act;
    }
    if has(Shape::ActTitle)
        && ins.category.as_deref() == Some("Legislation_and_supporting_material")
    {
        return Template::Act;
    }
    if has(Shape::BillTitle) || has(Shape::EmPhrase) {
        return Template::BillEm;
    }
    if ins.category.as_deref() == Some("Edited_private_advice") {
        return Template::Epa;
    }
    Template::Other
}

// ----- Token regexes (used by extractors to pull year/num) -----

pub(crate) fn re_citation_token() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(&format!(
            r"^({})\s+(?P<year>\d{{1,4}})/(?P<draft>D?)(?P<num>\d+)(?P<suffix>[A-Z0-9]*)",
            ruling_series_alt()
        ))
        .unwrap()
    })
}

pub(crate) fn re_atoid_token() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"^ATO\s+ID\s+(?P<year>\d{4})/(?P<num>\d+)(?P<suffix>[A-Z0-9]*)").unwrap()
    })
}

pub(crate) fn re_psla_token() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(
            r"^PS\s+LA\s+(?P<year>\d{4})/(?P<draft>D?)(?P<num>\d+)(?P<suffix>[A-Z0-9]*)",
        )
        .unwrap()
    })
}

pub(crate) fn re_smsfrb_token() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^SMSFRB\s+(?P<year>\d{4})/(?P<num>\d+)").unwrap())
}

pub(crate) fn re_neutral_token() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"^\[(?P<year>\d{4})\]\s+(?P<court>[A-Z]+)\s+(?P<num>\d+)").unwrap()
    })
}

pub(crate) fn re_act_year() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"\b(?P<year>(?:19|20)\d{2})\b").unwrap())
}

pub(crate) fn re_bill_year() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"\bBill\s+(?P<year>(?:19|20)\d{2})\b").unwrap())
}

pub(crate) fn re_withdrawn() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"(?i)\(\s*withdrawn\s*\)").unwrap())
}

pub(crate) fn re_precise_date() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(\d{1,2})\s+(January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{4})\b").unwrap()
    })
}

pub(crate) fn month_index(name: &str) -> u32 {
    match name.to_ascii_lowercase().as_str() {
        "january" => 1,
        "february" => 2,
        "march" => 3,
        "april" => 4,
        "may" => 5,
        "june" => 6,
        "july" => 7,
        "august" => 8,
        "september" => 9,
        "october" => 10,
        "november" => 11,
        "december" => 12,
        _ => 0,
    }
}

pub(crate) fn re_old_report() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"\((?P<year>1[89]\d{2}|20\d{2})\)\s+(?:L\.?R\.?|AC|QB|KB|Ch|CLR|ALR|ATC|ATR|FCR|HL|PC|NSWLR|VR|QR|SASR)").unwrap()
    })
}

pub(crate) fn re_mailto_body() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r#"MailTo:\?Subject=[^&]*&Body=([^)\s"]+)"#).unwrap())
}

pub(crate) fn re_case_header_name() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^\*##\s+(?P<name>[^*\n]+?)\s*\*").unwrap())
}

pub(crate) fn clean_citation(raw: &str) -> String {
    let cleaned = re_withdrawn().replace_all(raw, "").trim().to_string();
    let cleaned = regex::Regex::new(r"\s+")
        .unwrap()
        .replace_all(&cleaned, " ")
        .to_string();
    let pattern = format!(
        r"^({}|ATO\s+ID|PS\s+LA|SMSFRB)\s+(\d{{1,4}})/(D?)(\d+)([A-Z]{{1,2}}\d*)?$",
        ruling_series_alt()
    );
    let re = regex::Regex::new(&pattern).unwrap();
    if let Some(c) = re.captures(&cleaned) {
        let series = &c[1];
        let year = &c[2];
        let draft = &c[3];
        let num = &c[4];
        let suffix = c.get(5).map(|m| m.as_str()).unwrap_or("");
        return format!("{series} {year}/{draft}{num}{suffix}");
    }
    cleaned
}

pub(crate) fn year_from_token(token: &str) -> Option<u32> {
    let regs = [
        re_citation_token(),
        re_atoid_token(),
        re_psla_token(),
        re_smsfrb_token(),
        re_neutral_token(),
    ];
    for re in regs.iter() {
        if let Some(c) = re.captures(token) {
            if let Some(y) = c.name("year") {
                let s = y.as_str();
                let v: u32 = s.parse().ok()?;
                return Some(if s.len() == 4 { v } else { 1900 + v });
            }
        }
    }
    None
}

pub(crate) fn precise_date(text: &str) -> Option<String> {
    let m = re_precise_date().captures(text)?;
    let day: u32 = m.get(1)?.as_str().parse().ok()?;
    let month_name = m.get(2)?.as_str();
    let year: u32 = m.get(3)?.as_str().parse().ok()?;
    let month = month_index(month_name);
    if month == 0 {
        return None;
    }
    Some(format!("{:04}-{:02}-{:02}", year, month, day))
}

pub(crate) fn type_phrase_shape(s: Shape) -> bool {
    matches!(
        s,
        Shape::RulingTypePhrase
            | Shape::GuidelineTypePhrase
            | Shape::AtoidPhrase
            | Shape::PslaPhrase
            | Shape::SmsfrbPhrase
            | Shape::DisPhrase
            | Shape::AlertPhrase
            | Shape::EmPhrase
    )
}

pub(crate) fn citation_shape(s: Shape) -> bool {
    matches!(
        s,
        Shape::RulingCitation
            | Shape::RulingUnslashed
            | Shape::Atoid
            | Shape::Psla
            | Shape::Smsfrb
            | Shape::NeutralCitation
            | Shape::NameVName
            | Shape::ReX
            | Shape::CaseNumber
            | Shape::ActTitle
            | Shape::BillTitle
    )
}

pub(crate) fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<&str>>().join(" ")
}

pub(crate) fn compose_title(primary: Option<&str>, ins: &RuleInputs) -> Option<String> {
    let primary = primary?;
    if primary.is_empty() {
        return None;
    }
    let primary = collapse_ws(primary);
    let mut parts = vec![primary.clone()];
    let mut seen = std::collections::HashSet::new();
    seen.insert(primary.to_lowercase());
    for h in ins.headings.iter().take(5) {
        let t = collapse_ws(h);
        if t.is_empty() || seen.contains(&t.to_lowercase()) {
            continue;
        }
        if t.starts_with("/law/view/") {
            continue;
        }
        let s = shape_of(&t);
        if type_phrase_shape(s) || citation_shape(s) {
            continue;
        }
        parts.push(t);
        break;
    }
    Some(parts.join(" — "))
}

pub(crate) fn prefix_overlap(candidate: &str, parts: &[String]) -> bool {
    let cand_lower = candidate.to_lowercase();
    for p in parts {
        let p_lower = p.to_lowercase();
        if cand_lower == p_lower
            || cand_lower.starts_with(&p_lower)
            || p_lower.starts_with(&cand_lower)
        {
            return true;
        }
    }
    false
}

pub(crate) fn compose_from_em_front_matter(ins: &RuleInputs) -> Option<String> {
    let phrase = ins.front_matter_phrase.as_deref()?.trim().to_string();
    if phrase.is_empty() {
        return None;
    }
    let refs: Vec<&String> = ins
        .front_matter_refs
        .iter()
        .filter(|r| !r.trim().is_empty())
        .collect();
    if refs.is_empty() {
        return None;
    }
    let citation = collapse_ws(refs[0]);
    let mut parts = vec![phrase, citation];
    let mut section: Option<String> = None;
    for h in &ins.headings {
        let t = collapse_ws(h);
        if !t.is_empty() {
            section = Some(t);
            break;
        }
    }
    if let Some(s) = section {
        if !prefix_overlap(&s, &parts) {
            parts.push(s);
        }
    }
    Some(parts.join(" — "))
}

pub(crate) fn compose_from_body_h2(ins: &RuleInputs) -> Option<String> {
    if ins.headings.is_empty() || ins.heading_levels.len() != ins.headings.len() {
        return None;
    }
    for (i, lvl) in ins.heading_levels.iter().enumerate() {
        if *lvl == 1 && !collapse_ws(&ins.headings[i]).is_empty() {
            return None;
        }
    }
    for (i, lvl) in ins.heading_levels.iter().enumerate() {
        if *lvl == 2 {
            let t = collapse_ws(&ins.headings[i]);
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

pub(crate) fn compose_from_first_ref(ins: &RuleInputs) -> Option<String> {
    if ins.front_matter_phrase.is_some() {
        return None;
    }
    let first = ins
        .front_matter_refs
        .iter()
        .find(|r| !r.trim().is_empty())?;
    Some(collapse_ws(first))
}

pub(crate) fn compose_from_leading_headings(ins: &RuleInputs) -> Option<String> {
    if ins.headings.is_empty() || ins.heading_levels.len() != ins.headings.len() {
        return None;
    }
    let h1_idx = ins
        .heading_levels
        .iter()
        .enumerate()
        .find(|(i, lvl)| **lvl == 1 && !collapse_ws(&ins.headings[*i]).is_empty())
        .map(|(i, _)| i)?;
    let h1 = collapse_ws(&ins.headings[h1_idx]);
    let h2_idx = ((h1_idx + 1)..ins.headings.len())
        .find(|i| ins.heading_levels[*i] == 2 && !collapse_ws(&ins.headings[*i]).is_empty());
    let h3_anchor = h2_idx.unwrap_or(h1_idx);
    let h3_idx = ((h3_anchor + 1)..ins.headings.len())
        .find(|i| ins.heading_levels[*i] == 3 && !collapse_ws(&ins.headings[*i]).is_empty());
    let mut parts = vec![h1];
    for idx in [h2_idx, h3_idx].iter().flatten() {
        let candidate = collapse_ws(&ins.headings[*idx]);
        if prefix_overlap(&candidate, &parts) {
            continue;
        }
        parts.push(candidate);
    }
    Some(parts.join(" — "))
}

// ----- Per-template extractors -----

pub(crate) fn extract_official_pub(ins: &RuleInputs) -> DerivedMetadata {
    let mut citation_heading: Option<String> = None;
    let mut unslashed_heading: Option<String> = None;
    for h in ins.headings.iter().take(6) {
        let s = shape_of(h);
        if matches!(s, Shape::RulingCitation | Shape::Atoid | Shape::Psla) {
            citation_heading = Some(h.clone());
            break;
        }
        if unslashed_heading.is_none() && s == Shape::RulingUnslashed {
            unslashed_heading = Some(h.clone());
        }
    }
    if citation_heading.is_none() {
        if let Some(uh) = unslashed_heading {
            let t = collapse_ws(&uh);
            let trimmed = regex::Regex::new(r"\s*[—\-].*$")
                .unwrap()
                .replace(&t, "")
                .trim()
                .to_string();
            return DerivedMetadata {
                title: compose_title(Some(&trimmed), ins),
                date: precise_date(&ins.body_head.chars().take(600).collect::<String>()),
            };
        }
        return extract_other(ins);
    }
    let raw = citation_heading.unwrap();
    let mut cleaned = clean_citation(&raw);
    let year = year_from_token(&cleaned);
    let head_slice: String = ins.body_head.chars().take(600).collect();
    let pd = precise_date(&head_slice);
    if re_withdrawn().is_match(&raw) {
        cleaned = format!("{} (Withdrawn)", cleaned);
    }
    DerivedMetadata {
        title: compose_title(Some(&cleaned), ins),
        date: pd.or_else(|| year.map(|y| format!("{}-01-01", y))),
    }
}

pub(crate) fn case_name_from(heading: &str) -> Option<String> {
    let t = collapse_ws(heading);
    if t.is_empty() || t.len() > 200 {
        return None;
    }
    let t = regex::Regex::new(r"\s*\[\d{4}\].*$")
        .unwrap()
        .replace(&t, "")
        .trim()
        .to_string();
    let t = regex::Regex::new(r"\bv\.\s+")
        .unwrap()
        .replace_all(&t, "v ")
        .to_string();
    Some(t)
}

pub(crate) fn extract_case_h1(ins: &RuleInputs) -> DerivedMetadata {
    let mut name: Option<String> = None;
    let mut year: Option<u32> = None;
    for h in ins.headings.iter().take(5) {
        let s = shape_of(h);
        if s == Shape::NeutralCitation {
            if let Some(c) = re_neutral_token().captures(h.trim()) {
                let y_str = &c["year"];
                let court = &c["court"];
                let num = &c["num"];
                name = Some(format!("[{}] {} {}", y_str, court, num));
                year = y_str.parse().ok();
                break;
            }
        }
        if s == Shape::NameVName || s == Shape::ReX {
            name = case_name_from(h);
            break;
        }
        if s == Shape::CaseNumber {
            name = Some(collapse_ws(h));
            break;
        }
    }
    if name.is_none() {
        let em_dash_re = regex::Regex::new(r"\s+—\s+").unwrap();
        for h in ins.headings.iter().take(3) {
            for part in em_dash_re.split(h) {
                let part = collapse_ws(part);
                let ps = shape_of(&part);
                if ps == Shape::NameVName && part != *h {
                    name = case_name_from(&part);
                    break;
                }
                if ps == Shape::NeutralCitation {
                    if let Some(c) = re_neutral_token().captures(&part) {
                        let y_str = &c["year"];
                        name = Some(format!("[{}] {} {}", y_str, &c["court"], &c["num"]));
                        year = y_str.parse().ok();
                        break;
                    }
                }
            }
            if name.is_some() {
                break;
            }
        }
    }
    if name.is_none() && ins.category.as_deref() == Some("Cases") {
        for h in ins.headings.iter().take(3) {
            let clean = collapse_ws(h);
            if !clean.is_empty() && !clean.starts_with("/law/view/") && clean.len() < 200 {
                name = Some(clean);
                break;
            }
        }
    }
    if year.is_none() {
        let mut sources = vec![ins.title.clone().unwrap_or_default()];
        sources.extend(ins.headings.iter().take(5).cloned());
        for src in &sources {
            if let Some(c) = re_neutral_token().find(src) {
                if let Some(cap) = re_neutral_token().captures(c.as_str()) {
                    year = cap["year"].parse().ok();
                    break;
                }
            }
        }
    }
    if year.is_none() {
        let head: String = ins.body_head.chars().take(400).collect();
        if let Some(c) = re_old_report().captures(&head) {
            year = c["year"].parse().ok();
        }
    }
    let head: String = ins.body_head.chars().take(600).collect();
    let pd = precise_date(&head);
    DerivedMetadata {
        title: name,
        date: pd.or_else(|| year.map(|y| format!("{}-01-01", y))),
    }
}

pub(crate) fn extract_case_h2(ins: &RuleInputs) -> DerivedMetadata {
    let name = case_name_from(&ins.h2());
    let head: String = ins.body_head.chars().take(500).collect();
    let mut year: Option<u32> = None;
    if let Some(c) = re_neutral_token().captures(&head) {
        year = c["year"].parse().ok();
    }
    if year.is_none() {
        if let Some(c) = re_old_report().captures(&head) {
            year = c["year"].parse().ok();
        }
    }
    let head6: String = ins.body_head.chars().take(600).collect();
    DerivedMetadata {
        title: name,
        date: precise_date(&head6).or_else(|| year.map(|y| format!("{}-01-01", y))),
    }
}

pub(crate) fn extract_dis(ins: &RuleInputs) -> DerivedMetadata {
    let case_name = case_name_from(&ins.h2()).or_else(|| case_name_from(&ins.h1()));
    let head: String = ins.body_head.chars().take(1200).collect();
    let mut year: Option<u32> = None;
    if let Some(c) = re_neutral_token().captures(&head) {
        year = c["year"].parse().ok();
    }
    let head6: String = ins.body_head.chars().take(600).collect();
    let pd = precise_date(&head6);
    let title = case_name.map(|n| format!("DIS: {}", n));
    DerivedMetadata {
        title,
        date: pd.or_else(|| year.map(|y| format!("{}-01-01", y))),
    }
}

pub(crate) fn extract_act(ins: &RuleInputs) -> DerivedMetadata {
    let name = collapse_ws(&ins.h1());
    let year = re_act_year()
        .captures(&name)
        .and_then(|c| c["year"].parse().ok());
    DerivedMetadata {
        title: if name.is_empty() { None } else { Some(name) },
        date: year.map(|y: u32| format!("{}-01-01", y)),
    }
}

pub(crate) fn parse_mailto_body(body_head: &str) -> Vec<String> {
    let m = match re_mailto_body().captures(body_head) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let raw = &m[1];
    let parts = raw.split("%0D");
    let mut out = Vec::new();
    for p in parts {
        // Manual percent-decode (matches the helper used elsewhere).
        let bytes = p.as_bytes();
        let mut decoded = String::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(byte) = u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"),
                    16,
                ) {
                    decoded.push(byte as char);
                    i += 3;
                    continue;
                }
            }
            decoded.push(bytes[i] as char);
            i += 1;
        }
        let text = decoded.trim().to_string();
        if text.is_empty() || text.to_lowercase().starts_with("link:") {
            continue;
        }
        out.push(text);
    }
    out
}

pub(crate) fn extract_legislation_section(ins: &RuleInputs) -> DerivedMetadata {
    let inner = ins.inner_body();
    let cap = re_docid_act_section().captures(&inner);
    let year = cap.as_ref().and_then(|c| c[1].parse::<u32>().ok());
    let act_no = cap
        .as_ref()
        .map(|c| c[2].trim_start_matches('0').to_string());
    let segs: Vec<&str> = ins.doc_id.split('/').filter(|s| !s.is_empty()).collect();
    let section_id = segs.get(2).map(|s| s.to_string()).unwrap_or_default();
    let outer = ins.outer_prefix();

    let mut act_name: Option<String> = None;
    for h in ins.headings.iter().take(6) {
        let t = collapse_ws(h);
        if re_act_title().is_match(&t) {
            act_name = Some(t);
            break;
        }
    }
    if act_name.is_none() {
        for line in parse_mailto_body(&ins.body_head) {
            if re_act_title().is_match(&line) {
                act_name = Some(line);
                break;
            }
        }
    }
    let title = if let Some(n) = act_name.clone() {
        if !section_id.is_empty() {
            if outer == "PAC" {
                format!("{} s {}", n, section_id)
            } else {
                format!("{} reg {}", n, section_id)
            }
        } else {
            n
        }
    } else if outer == "PAC" {
        match (year, act_no.as_deref()) {
            (Some(y), Some(no)) => format!("Act {} No. {} s {}", y, no, section_id),
            _ => format!("PAC {}/{}", inner, section_id),
        }
    } else {
        match year {
            Some(y) => format!("Regulations {} reg {}", y, section_id),
            None => format!("REG {}/{}", inner, section_id),
        }
    };
    let final_year = year.or_else(|| {
        act_name
            .as_ref()
            .and_then(|n| re_act_year().captures(n))
            .and_then(|c| c["year"].parse().ok())
    });
    let head6: String = ins.body_head.chars().take(600).collect();
    DerivedMetadata {
        title: Some(title),
        date: precise_date(&head6)
            .or_else(|| final_year.map(|y| format!("{}-01-01", y)))
            .or_else(|| ins.pub_date.clone()),
    }
}

pub(crate) fn extract_historical_case(ins: &RuleInputs) -> DerivedMetadata {
    let inner = ins.inner_body();
    let year = re_docid_jud_star()
        .captures(&inner)
        .and_then(|c| c[1].parse::<u32>().ok());
    let head4: String = ins.body_head.chars().take(400).collect();
    let mut name: Option<String> = None;
    if let Some(c) = re_case_header_name().captures(&head4) {
        name = Some(collapse_ws(&c["name"]));
    }
    if name.is_none() {
        let trail_re = regex::Regex::new(r"\s*-\s*\([^)]+\)\s*$").unwrap();
        for line in parse_mailto_body(&ins.body_head) {
            if line.to_lowercase() == "cases" {
                continue;
            }
            if line.contains(" v ") || line.contains(" - (") {
                let nm = trail_re.replace(&line, "").trim().to_string();
                if !nm.is_empty() && nm.len() < 200 {
                    name = Some(nm);
                    break;
                }
            }
        }
    }
    if name.is_none() {
        for h in ins.headings.iter().take(4) {
            let t = collapse_ws(h);
            if !t.is_empty() && !t.starts_with("/law/view/") && t.len() < 200 {
                name = Some(t);
                break;
            }
        }
    }
    if name.is_none() {
        name = if inner.is_empty() { None } else { Some(inner) };
    }
    let head6: String = ins.body_head.chars().take(600).collect();
    DerivedMetadata {
        title: name,
        date: precise_date(&head6)
            .or_else(|| year.map(|y| format!("{}-01-01", y)))
            .or_else(|| ins.pub_date.clone()),
    }
}

pub(crate) fn extract_bill_em(ins: &RuleInputs) -> DerivedMetadata {
    let em_title = compose_from_em_front_matter(ins);
    let h2 = ins.h2();
    let h1 = ins.h1();
    let source = if re_bill_year().is_match(&h2) { h2 } else { h1 };
    let mut bill_title = collapse_ws(&source);
    if !re_bill_year().is_match(&bill_title) && !re_act_title().is_match(&bill_title) {
        let head8: String = ins.body_head.chars().take(800).collect();
        let bold_re = regex::Regex::new(r"\*\*([^*]+?)\*\*").unwrap();
        for cap in bold_re.captures_iter(&head8) {
            let line = collapse_ws(&cap[1]);
            if re_bill_year().is_match(&line) || re_act_title().is_match(&line) {
                bill_title = line;
                break;
            }
        }
    }
    let year = re_bill_year()
        .captures(&bill_title)
        .or_else(|| re_act_year().captures(&bill_title))
        .and_then(|c| c["year"].parse::<u32>().ok());
    let mut title = em_title;
    if title.is_none() && !bill_title.is_empty() {
        title = if !bill_title.contains("Explanatory") && year.is_some() {
            Some(format!("EM to {}", bill_title))
        } else {
            Some(bill_title.clone())
        };
    }
    let needs_compose = title
        .as_deref()
        .map(|t| type_phrase_shape(shape_of(t)))
        .unwrap_or(true);
    if needs_compose {
        if let Some(c) = compose_from_leading_headings(ins)
            .or_else(|| compose_from_body_h2(ins))
            .or_else(|| compose_from_first_ref(ins))
        {
            title = Some(c);
        }
    }
    let head6: String = ins.body_head.chars().take(600).collect();
    DerivedMetadata {
        title,
        date: precise_date(&head6)
            .or_else(|| year.map(|y| format!("{}-01-01", y)))
            .or_else(|| ins.pub_date.clone()),
    }
}

pub(crate) fn extract_smsfrb(ins: &RuleInputs) -> DerivedMetadata {
    for h in ins.headings.iter().take(4) {
        if let Some(c) = re_smsfrb_token().captures(h) {
            let year: u32 = c["year"].parse().unwrap_or(0);
            return DerivedMetadata {
                title: compose_title(Some(&format!("SMSFRB {}/{}", &c["year"], &c["num"])), ins),
                date: Some(format!("{}-01-01", year)),
            };
        }
    }
    extract_other(ins)
}

pub(crate) fn re_docid_year4() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(&format!(
            r"^({})(?P<year>(?:19|20)\d{{2}})(?P<draft>D?)(?P<num>\d+)$",
            ruling_series_alt()
        ))
        .unwrap()
    })
}

pub(crate) fn re_docid_year2() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(&format!(
            r"^({})(?P<year>[89]\d)(?P<num>\d+)$",
            ruling_series_alt()
        ))
        .unwrap()
    })
}

pub(crate) fn re_docid_psla() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^PSLA(?P<year>\d{4})(?P<num>\d+)$").unwrap())
}

pub(crate) fn re_docid_psla_draft() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^PSD(?P<year>\d{4})D?(?P<num>\d+)$").unwrap())
}

pub(crate) fn re_docid_atoid() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^(?:ATOID|AID)(?P<year>\d{4})(?P<num>\d+)$").unwrap())
}

pub(crate) fn extract_from_docid(ins: &RuleInputs) -> (Option<String>, Option<u32>) {
    let body = ins.inner_body();
    if let Some(c) = re_docid_year4().captures(&body) {
        let series = &c[1];
        let y: u32 = c["year"].parse().unwrap_or(0);
        let draft = &c["draft"];
        return (
            Some(format!("{} {}/{}{}", series, &c["year"], draft, &c["num"])),
            Some(y),
        );
    }
    if let Some(c) = re_docid_psla().captures(&body) {
        let y: u32 = c["year"].parse().unwrap_or(0);
        return (Some(format!("PS LA {}/{}", &c["year"], &c["num"])), Some(y));
    }
    if let Some(c) = re_docid_psla_draft().captures(&body) {
        let y: u32 = c["year"].parse().unwrap_or(0);
        return (
            Some(format!("PS LA {}/D{}", &c["year"], &c["num"])),
            Some(y),
        );
    }
    if let Some(c) = re_docid_atoid().captures(&body) {
        let y: u32 = c["year"].parse().unwrap_or(0);
        return (
            Some(format!("ATO ID {}/{}", &c["year"], &c["num"])),
            Some(y),
        );
    }
    if let Some(c) = re_docid_year2().captures(&body) {
        let series = &c[1];
        let y2: u32 = c["year"].parse().unwrap_or(0);
        return (
            Some(format!("{} {}/{}", series, &c["year"], &c["num"])),
            Some(1900 + y2),
        );
    }
    (None, None)
}

pub(crate) fn re_date_of_advice() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"(?i)\bDate\s+of\s+(?:advice|ruling|issue)\s*[:\-]?\s*(?P<day>\d{1,2})\s+(?P<mon>January|February|March|April|May|June|July|August|September|October|November|December)\s+(?P<year>\d{4})").unwrap()
    })
}

pub(crate) fn extract_epa(ins: &RuleInputs) -> DerivedMetadata {
    let auth = ins.inner_body();
    let auth = auth.trim().to_string();
    let outer = ins.outer_prefix();
    let code = if !auth.is_empty() {
        Some(format!("{} {}", outer, auth))
    } else {
        None
    };
    let head: String = ins.body_head.chars().take(1500).collect();
    let mut precise: Option<String> = None;
    if let Some(c) = re_date_of_advice().captures(&head) {
        let month = month_index(&c["mon"]);
        let day: u32 = c["day"].parse().unwrap_or(0);
        let year: u32 = c["year"].parse().unwrap_or(0);
        precise = Some(format!("{:04}-{:02}-{:02}", year, month, day));
    }
    let date = precise.or_else(|| ins.pub_date.clone());
    DerivedMetadata { title: code, date }
}

pub(crate) fn extract_other(ins: &RuleInputs) -> DerivedMetadata {
    let (code, year) = extract_from_docid(ins);
    let mut year = year;
    if year.is_none() {
        if let Some(pd) = ins.pub_date.as_deref() {
            if pd.len() >= 4 {
                let prefix = &pd[..4];
                if prefix.chars().all(|c| c.is_ascii_digit()) {
                    year = prefix.parse().ok();
                }
            }
        }
    }
    let head6: String = ins.body_head.chars().take(600).collect();
    let pd = precise_date(&head6);
    let title = compose_from_em_front_matter(ins)
        .or_else(|| compose_from_leading_headings(ins))
        .or_else(|| compose_from_body_h2(ins))
        .or_else(|| compose_from_first_ref(ins))
        .or(code);
    DerivedMetadata {
        title,
        date: pd
            .or_else(|| ins.pub_date.clone())
            .or_else(|| year.map(|y| format!("{}-01-01", y))),
    }
}

pub(crate) fn universal_fallback_title(ins: &RuleInputs) -> Option<String> {
    let outer = ins.outer_prefix();
    let inner = ins.inner_body();
    if !outer.is_empty() && !inner.is_empty() {
        return Some(format!("{} {}", outer, inner));
    }
    if !outer.is_empty() {
        return Some(outer);
    }
    None
}

pub(crate) fn year_from_docid_fallback(ins: &RuleInputs) -> Option<u32> {
    let body = ins.inner_body();
    if let Some(c) = re_docid_jud_star().captures(&body) {
        return c[1].parse().ok();
    }
    if let Some(c) = re_docid_act_section().captures(&body) {
        return c[1].parse().ok();
    }
    let r = regex::Regex::new(r"^((?:19|20)\d{2})").unwrap();
    if let Some(c) = r.captures(&body) {
        return c[1].parse().ok();
    }
    None
}

pub(crate) fn derive_metadata(ins: &RuleInputs) -> DerivedMetadata {
    let template = classify(ins);
    let mut result = match template {
        Template::OfficialPub => extract_official_pub(ins),
        Template::CaseH1 => extract_case_h1(ins),
        Template::CaseH2 => extract_case_h2(ins),
        Template::HistCase => extract_historical_case(ins),
        Template::Dis => extract_dis(ins),
        Template::Act => extract_act(ins),
        Template::LegislationSection => extract_legislation_section(ins),
        Template::BillEm => extract_bill_em(ins),
        Template::Smsfrb => extract_smsfrb(ins),
        Template::Epa => extract_epa(ins),
        Template::Other => extract_other(ins),
    };
    if result.title.is_none() {
        let (fb_code, fb_year) = extract_from_docid(ins);
        if let Some(c) = fb_code {
            result.title = Some(c);
            if result.date.is_none() {
                if let Some(y) = fb_year {
                    result.date = Some(format!("{}-01-01", y));
                }
            }
        }
    }
    if result.title.is_none() {
        result.title = universal_fallback_title(ins);
    }
    if result.date.is_none() {
        if let Some(pd) = ins.pub_date.clone() {
            result.date = Some(pd);
        } else if let Some(y) = year_from_docid_fallback(ins) {
            result.date = Some(format!("{}-01-01", y));
        }
    }
    result
}

#[cfg(test)]
mod rules_tests {
    use super::*;

    fn ins(doc_id: &str, headings: &[&str]) -> RuleInputs {
        RuleInputs {
            doc_id: doc_id.to_string(),
            headings: headings.iter().map(|s| s.to_string()).collect(),
            heading_levels: (1..=headings.len() as u32).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn shape_classifies_taxation_ruling_phrase() {
        assert_eq!(shape_of("Taxation Ruling"), Shape::RulingTypePhrase);
    }

    #[test]
    fn shape_classifies_ruling_citation() {
        assert_eq!(shape_of("TR 2024/3"), Shape::RulingCitation);
    }

    #[test]
    fn shape_classifies_neutral_citation() {
        assert_eq!(shape_of("[2024] HCA 41"), Shape::NeutralCitation);
    }

    #[test]
    fn classify_ruling_routes_to_official_pub() {
        let i = ins(
            "TXR/TR20243/NAT/ATO/00001",
            &["Taxation Ruling", "TR 2024/3", "Subtitle"],
        );
        assert_eq!(classify(&i), Template::OfficialPub);
    }

    #[test]
    fn derive_metadata_official_pub_title_with_citation() {
        let i = ins(
            "TXR/TR20243/NAT/ATO/00001",
            &[
                "Taxation Ruling",
                "TR 2024/3",
                "R&D tax incentive eligibility",
            ],
        );
        let d = derive_metadata(&i);
        assert_eq!(
            d.title.as_deref(),
            Some("TR 2024/3 — R&D tax incentive eligibility")
        );
        assert_eq!(d.date.as_deref(), Some("2024-01-01"));
    }

    #[test]
    fn derive_metadata_dis() {
        let mut i = ins(
            "DIS/DIS2024_PEPSICO/NAT/ATO",
            &[
                "Decision impact statement",
                "Pepsico Inc v Commissioner of Taxation",
            ],
        );
        i.body_head = String::new();
        let d = derive_metadata(&i);
        assert_eq!(
            d.title.as_deref(),
            Some("DIS: Pepsico Inc v Commissioner of Taxation")
        );
    }

    #[test]
    fn derive_metadata_act_year() {
        // PAC/<8digit>/<section> → LegislationSection extractor (not Act).
        // The Act name comes from h1 and gets " s <section>" appended.
        let i = ins("PAC/19970038/995-1", &["Income Tax Assessment Act 1997"]);
        let d = derive_metadata(&i);
        assert_eq!(
            d.title.as_deref(),
            Some("Income Tax Assessment Act 1997 s 995-1")
        );
        assert_eq!(d.date.as_deref(), Some("1997-01-01"));
    }

    #[test]
    fn derive_metadata_act_template_no_section() {
        // Pure Act title with no PAC docid → Act extractor.
        let i = ins(
            "ACT/INCOME_TAX_ASSESSMENT_1997",
            &["Income Tax Assessment Act 1997"],
        );
        let d = derive_metadata(&i);
        assert_eq!(d.title.as_deref(), Some("Income Tax Assessment Act 1997"));
        assert_eq!(d.date.as_deref(), Some("1997-01-01"));
    }

    #[test]
    fn derive_metadata_epa_uses_docid() {
        let mut i = ins("EV/1012101718232/00001", &[]);
        i.category = Some("Edited_private_advice".to_string());
        let d = derive_metadata(&i);
        assert_eq!(d.title.as_deref(), Some("EV 1012101718232"));
    }

    #[test]
    fn derive_metadata_universal_fallback_when_nothing_matches() {
        let i = ins("XYZ/abc/def", &[]);
        let d = derive_metadata(&i);
        assert_eq!(d.title.as_deref(), Some("XYZ abc"));
    }

    #[test]
    fn precise_date_parses_real_date() {
        assert_eq!(
            precise_date("issued on 12 March 2024 by ..."),
            Some("2024-03-12".to_string())
        );
    }

    #[test]
    fn clean_citation_drops_withdrawn_marker() {
        assert_eq!(clean_citation("LCR 2019/2EC (Withdrawn)"), "LCR 2019/2EC");
    }
}
