//! Payment-context guard: decides, before an agent browser action runs, whether it may pay or commit money or consent
//! (checkout, place order, subscribe, authorize ...). Heuristic text matching on data the page itself provides:
//! no network, no model. The tools ask the person when `assess` says so (see `synthesis-tools` browser.rs).
//!
//! shortcut: phrase tables, not understanding. A page can hide its intent (an icon-only button, a label like "OK",
//! a payment UI inside a cross-origin iframe, a language not in the tables). Extend the tables below to widen it.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::json;

use crate::{BrowserRuntime, Result};

/// Host suffixes of payment providers (a page on one is a payment page).
const PSP_HOSTS: &[&str] = &[
    "stripe.com",
    "paypal.com",
    "adyen.com",
    "checkout.com",
    "braintreegateway.com",
    "braintree-api.com",
    "squareup.com",
    "shop.app",
    "pay.google.com",
    "payments.google.com",
    "pay.apple.com",
    "momo.vn",
    "vnpay.vn",
    "zalopay.vn",
];
/// Phrases of the page PATH (words of the path, folded) that make it a payment page.
const PAGE_PATHS: &[&str] = &[
    "checkout",
    "checkouts",
    "pay",
    "payment",
    "payments",
    "billing",
    "order",
    "cart",
    "purchase",
    "subscribe",
    "donate",
    "oauth",
    "authorize",
    "consent",
    "confirm",
    "thanh toan",
    "dat hang",
    "gio hang",
];
/// Labels of an interactive element that pay or commit consent, on any page.
const ACTIONS: &[&str] = &[
    "pay",
    "pay now",
    "pay with",
    "place order",
    "place your order",
    "buy",
    "buy now",
    "purchase",
    "complete purchase",
    "complete order",
    "subscribe",
    "confirm payment",
    "confirm purchase",
    "confirm order",
    "confirm transfer",
    "donate",
    "authorize",
    "authorise",
    "allow access",
    "transfer",
    "send money",
    "checkout",
    "check out",
    "proceed to checkout",
    "thanh toan",
    "dat hang",
    "mua ngay",
    "chuyen tien",
    "xac nhan thanh toan",
    "xac nhan dat hang",
];
/// Generic labels that count only on a payment page, and only as the WHOLE label ("Continue shopping" is not "Continue").
const ACTIONS_IN_CONTEXT: &[&str] = &[
    "confirm", "continue", "submit", "accept", "agree", "allow", "approve", "next", "complete",
    "done", "i agree", "xac nhan", "tiep tuc", "dong y", "dang ky", "hoan tat",
];
/// Labels that are not an action although they contain one of the words above.
const NOT_ACTIONS: &[&str] = &["pay attention"];
/// Names of a card field (besides `autocomplete="cc-*"`).
const CARD_FIELDS: &[&str] = &[
    "card number",
    "cvc",
    "cvv",
    "security code",
    "expiry",
    "expiration",
    "so the",
    "ngay het han",
];

/// What the action does to the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A click on an element (by ref or role+name).
    Click,
    /// A click, double click, right click or drag by coordinates, or on the page generally: cannot be vetted.
    Blind,
    /// Text into a field.
    Fill,
    /// Enter or Space on the focused element.
    Key,
}

/// Where the action lands.
#[derive(Clone, Debug)]
pub enum PayTarget {
    Ref(String),
    Point(f64, f64),
    Focused,
}

/// What the page says about the target, read right before the action (page data: never trusted as instructions).
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub href: String,
    pub label: String,
    pub tag: String,
    pub role: String,
    pub typ: String,
    pub autocomplete: String,
    pub interactive: bool,
    pub card_inputs: bool,
    pub amount_text: String,
    pub frame_src: String,
    /// For a key press: the label of the submit control of the focused element's form.
    pub submit_label: String,
}

/// `fold` with only letters, digits and single spaces: ` pay now ` matches the phrase `pay now` on word borders.
fn words(s: &str) -> String {
    let f = fold(s);
    let mut out = String::from(" ");
    for w in f
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        out.push_str(w);
        out.push(' ');
    }
    out
}

/// Lowercase without diacritics (Vietnamese and common Latin ones; combining marks dropped).
pub fn fold(s: &str) -> String {
    const MAP: [(char, &str); 7] = [
        ('a', "àáạảãâầấậẩẫăằắặẳẵäåā"),
        ('e', "èéẹẻẽêềếệểễëē"),
        ('i', "ìíịỉĩïī"),
        ('o', "òóọỏõôồốộổỗơờớợởỡöō"),
        ('u', "ùúụủũưừứựửữüū"),
        ('y', "ỳýỵỷỹ"),
        ('d', "đ"),
    ];
    s.chars()
        .filter(|c| {
            !('\u{300}'..='\u{36f}').contains(c) // combining marks
                && !matches!(*c, '\u{ad}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}')
        })
        .flat_map(char::to_lowercase)
        .map(|c| {
            MAP.iter()
                .find(|(_, set)| set.contains(c))
                .map_or(c, |m| m.0)
        })
        .collect()
}

fn has_phrase(text: &str, table: &[&str]) -> bool {
    let w = words(text);
    table.iter().any(|p| w.contains(&format!(" {p} ")))
}

/// A label counts when short (a button's, not a paragraph's) and not on the exceptions list.
fn label_hit(label: &str, table: &[&str]) -> bool {
    label.chars().count() <= 80 && !has_phrase(label, NOT_ACTIONS) && has_phrase(label, table)
}

fn whole_label(label: &str, table: &[&str]) -> bool {
    let w = words(label);
    table.iter().any(|p| w == format!(" {p} "))
}

/// The page's host or path looks like a payment page.
pub fn page_hit(href: &str) -> bool {
    let Ok(u) = url::Url::parse(href) else {
        return false;
    };
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    PSP_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
        || [Some(u.path()), u.query(), u.fragment()] // a hash-routed SPA keeps its route in the fragment
            .into_iter()
            .flatten()
            .any(|part| has_phrase(part, PAGE_PATHS))
}

/// Why the action needs the person, or `None`. `page_ctx`: the page host/path matched or it has card inputs.
pub fn assess(p: &Probe, kind: Kind) -> Option<&'static str> {
    let page_ctx = page_hit(&p.href) || p.card_inputs;
    let frame_psp = !p.frame_src.is_empty() && page_hit(&p.frame_src);
    let act = p.interactive
        && (label_hit(&p.label, ACTIONS)
            || (page_ctx && whole_label(&p.label, ACTIONS_IN_CONTEXT)));
    match kind {
        Kind::Fill
            if p.autocomplete.to_ascii_lowercase().starts_with("cc-")
                || label_hit(&p.label, CARD_FIELDS) =>
        {
            Some("fills a card field")
        }
        Kind::Fill => None,
        Kind::Blind if page_ctx || frame_psp => {
            Some("a pointer action by position on a payment page")
        }
        Kind::Key if page_ctx => Some("a key press on a payment page"),
        Kind::Key if label_hit(&p.submit_label, ACTIONS) => {
            Some("the key submits a form whose button pays or commits")
        }
        Kind::Click | Kind::Blind | Kind::Key if act || frame_psp => {
            Some("the target looks like a pay or confirm control")
        }
        _ => None,
    }
}

/// Same target and page (the amount text may change, a timer may tick).
pub fn same_target(a: &Probe, b: &Probe) -> bool {
    let o = |h: &str| {
        url::Url::parse(h).map_or(String::new(), |u| {
            format!("{}{}", u.origin().ascii_serialization(), u.path())
        })
    };
    o(&a.href) == o(&b.href)
        && (&a.label, &a.tag, &a.role, &a.typ) == (&b.label, &b.tag, &b.role, &b.typ)
}

/// Printable, one line, at most `max` characters (page text shown to the person).
pub fn clean(s: &str, max: usize) -> String {
    let one: String = s
        .chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect();
    let one = one.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > max {
        format!("{}...", one.chars().take(max).collect::<String>())
    } else {
        one
    }
}

/// An amount near the target, if the text near it has one (the line with "total" first). Optional.
pub fn amount(text: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)[$€£¥₫]\s?\d|\d[\d.,]*\s?(?:[$€£¥₫đ]|usd|eur|gbp|vnd)")
            .expect("static pattern")
    });
    let hits: Vec<&str> = text.lines().filter(|l| re.is_match(l)).collect();
    let total = hits
        .iter()
        .find(|l| has_phrase(l, &["total", "tong", "amount due"]));
    total.or(hits.first()).map(|l| clean(l, 60))
}

/// Reads the target: a function run in the page on the element (the label, its role, the page's card inputs, the text
/// near it). `el` is `null` when nothing is there.
const PROBE_JS: &str = r#"function(el){
const d=document,t=s=>String(s||'').replace(/\s+/g,' ').trim().slice(0,200);
if(el===d.body||el===d.documentElement)el=null;
let it=el&&el.closest&&el.closest('button,a,input,select,summary,label,[role=button],[role=link],[role=menuitem],[onclick]');
if(el&&!it&&getComputedStyle(el).cursor==='pointer')it=el;
const n=it||el;let label='',amt='',sub='';
if(n){const g=a=>n.getAttribute(a)||'',tg=n.tagName,field=/^(INPUT|TEXTAREA|SELECT)$/.test(tg)||n.isContentEditable;
const by=g('aria-labelledby').split(' ').map(i=>d.getElementById(i)).filter(Boolean).map(e=>e.textContent).join(' ');
// never what the person or the agent typed: a field's value and a contenteditable's text stay out of the label
const btn=tg==='INPUT'&&/^(button|submit|reset|image)$/i.test(n.type)?n.value:'';
label=t(g('aria-label')||by||(field?'':n.innerText)||btn||(n.labels&&n.labels[0]&&n.labels[0].innerText)||g('title')||g('alt')||g('placeholder')||g('name'));
for(let a=n,i=0;a&&i<5;a=a.parentElement,i++){const x=a.innerText||'';if(/\d/.test(x)&&/[$€£¥₫]|\d\s?(đ|vnd|usd|eur|gbp)/i.test(x)){amt=x.slice(0,800);break}}
const f=n.form,sb=f&&f.querySelector('button[type=submit],input[type=submit],button:not([type])');
if(sb)sub=t(sb.innerText||sb.value||sb.getAttribute('aria-label'))}
const g=a=>n?n.getAttribute(a)||'':'';
return{href:location.href,label,tag:n?n.tagName.toLowerCase():'',role:g('role'),typ:g('type'),ac:g('autocomplete'),
interactive:!!it,card:!!d.querySelector('[autocomplete^="cc-"]'),amt,sub,src:n&&n.tagName==='IFRAME'?n.src:''}}"#;

impl BrowserRuntime {
    /// Reads `target` for `assess` (one page call, in an ISOLATED world: the page's own scripts cannot replace the DOM
    /// methods it uses). Every field is clipped here before anything folds or shows it. An error means detection
    /// could not run here.
    pub async fn payment_probe(&self, target: &PayTarget) -> Result<Probe> {
        let tab = self.current()?;
        let frame = self.call(&tab, "Page.getFrameTree", json!({})).await?;
        let frame = frame["frameTree"]["frame"]["id"]
            .as_str()
            .ok_or(crate::BrowserError::Closed)?
            .to_owned();
        let world = self
            .call(
                &tab,
                "Page.createIsolatedWorld",
                json!({"frameId": frame, "worldName": "synthesis-payment"}),
            )
            .await?;
        let ctx = world["executionContextId"]
            .as_i64()
            .ok_or_else(|| crate::BrowserError::Page("no isolated world".into()))?;
        let v = match target {
            PayTarget::Ref(r) => {
                let oid = self.isolated_object(&tab, r).await?;
                let f = format!("function(){{return ({PROBE_JS})(this)}}");
                let r = self
                    .call(
                        &tab,
                        "Runtime.callFunctionOn",
                        json!({"objectId": oid, "functionDeclaration": f, "returnByValue": true}),
                    )
                    .await?;
                r["result"]["value"].clone()
            }
            PayTarget::Point(x, y) => {
                let at = format!("document.elementFromPoint({}, {})", x.round(), y.round());
                self.eval_in(&tab, ctx, &format!("({PROBE_JS})({at})"))
                    .await?
            }
            PayTarget::Focused => {
                self.eval_in(&tab, ctx, &format!("({PROBE_JS})(document.activeElement)"))
                    .await?
            }
        };
        let s = |k: &str, max: usize| {
            v[k].as_str()
                .unwrap_or("")
                .chars()
                .take(max)
                .collect::<String>()
        };
        if s("href", 1).is_empty() {
            return Err(crate::BrowserError::Page(
                "payment probe returned nothing".into(),
            ));
        }
        Ok(Probe {
            href: s("href", 2048),
            label: s("label", 200),
            tag: s("tag", 30),
            role: s("role", 30),
            typ: s("typ", 30),
            autocomplete: s("ac", 60),
            interactive: v["interactive"] == true,
            card_inputs: v["card"] == true,
            amount_text: s("amt", 800),
            frame_src: s("src", 2048),
            submit_label: s("sub", 200),
        })
    }

    async fn eval_in(&self, tab: &crate::Tab, ctx: i64, expr: &str) -> Result<serde_json::Value> {
        let r = self
            .call(
                tab,
                "Runtime.evaluate",
                json!({"expression": expr, "contextId": ctx, "returnByValue": true}),
            )
            .await?;
        if r.get("exceptionDetails").is_some() {
            return Err(crate::BrowserError::Page("payment probe failed".into()));
        }
        Ok(r["result"]["value"].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(href: &str, label: &str, interactive: bool) -> Probe {
        Probe {
            href: href.into(),
            label: label.into(),
            interactive,
            ..Default::default()
        }
    }

    #[test]
    fn labels_that_pay_or_commit_are_hits_and_others_are_not() {
        let shop = "https://shop.example/products/1";
        for l in [
            "Place order",
            "PAY NOW",
            "Buy now",
            "Subscribe",
            "Donate",
            "Authorize",
            "Allow access",
            "Send money",
            "Thanh toán",
            "Đặt hàng",
            "MUA NGAY",
            "Chuyển tiền",
            "Proceed to checkout",
            "Pay $12.00",
            "Xác nhận thanh toán",
        ] {
            assert!(assess(&p(shop, l, true), Kind::Click).is_some(), "{l}");
        }
        for l in [
            "Continue shopping",
            "Add to cart",
            "Order history",
            "Search",
            "Allow all cookies",
            "Xác nhận",
        ] {
            assert!(assess(&p(shop, l, true), Kind::Click).is_none(), "{l}");
        }
        // not an interactive element (a heading), and the exceptions list
        assert!(assess(&p(shop, "Pay attention to this", false), Kind::Click).is_none());
        assert!(assess(&p(shop, "Pay attention", true), Kind::Click).is_none());
        assert!(assess(&p(shop, "Pay", false), Kind::Click).is_none());
        // a paragraph is not a button
        let long = "buy ".repeat(30);
        assert!(assess(&p(shop, &long, true), Kind::Click).is_none());
    }

    #[test]
    fn generic_labels_count_only_on_a_payment_page() {
        let on = p("https://shop.example/checkout", "Continue shopping", true);
        assert!(assess(&on, Kind::Click).is_none());
        for l in ["Continue", "Accept", "Đồng ý", "Xác nhận", "I agree"] {
            assert!(
                assess(&p("https://shop.example/", l, true), Kind::Click).is_none(),
                "{l}"
            );
            assert!(
                assess(
                    &p("https://shop.example/checkout/step2", l, true),
                    Kind::Click
                )
                .is_some(),
                "{l}"
            );
        }
        let mut cards = p("https://shop.example/x", "Continue", true);
        cards.card_inputs = true;
        assert!(assess(&cards, Kind::Click).is_some());
    }

    #[test]
    fn pages_by_host_and_path() {
        for u in [
            "https://checkout.stripe.com/c/pay/cs_1",
            "https://www.paypal.com/x",
            "https://x.adyen.com/",
            "https://shop.example/checkout",
            "https://shop.example/Cart/confirm",
            "https://shop.example/thanh-toan",
            "https://shop.example/gio-hang",
            "https://pay.google.com/",
        ] {
            assert!(page_hit(u), "{u}");
        }
        for u in [
            "https://example.com/",
            "https://shop.example/products/orderly",
            "https://notpaypal.com/",
            "https://example.com/blog/payroll",
            "not a url",
        ] {
            assert!(!page_hit(u), "{u}");
        }
    }

    #[test]
    fn coordinates_and_keys_on_a_payment_page_cannot_be_vetted() {
        let pay = "https://shop.example/checkout";
        let none = p(pay, "", false);
        assert!(assess(&none, Kind::Blind).is_some());
        assert!(assess(&none, Kind::Key).is_some());
        assert!(assess(&p("https://example.com/", "", false), Kind::Blind).is_none());
        assert!(assess(&p("https://example.com/", "", false), Kind::Key).is_none());
        // a blind click that lands on a pay button anywhere
        assert!(assess(&p("https://example.com/", "Buy now", true), Kind::Blind).is_some());
    }

    #[test]
    fn card_fields_are_asked_before_fill() {
        let mut f = p("https://example.com/", "", true);
        f.autocomplete = "cc-number".into();
        assert!(assess(&f, Kind::Fill).is_some());
        assert!(assess(&p("https://example.com/", "Card number", true), Kind::Fill).is_some());
        assert!(
            assess(
                &p("https://example.com/checkout", "Email", true),
                Kind::Fill
            )
            .is_none()
        );
    }

    #[test]
    fn folding_clean_amount_and_same_target() {
        assert_eq!(fold("ĐẶT HÀNG Thanh Toán"), "dat hang thanh toan");
        assert_eq!(fold("de\u{301}"), "de");
        assert_eq!(clean("a\u{7}b\n  c", 80), "ab c");
        assert_eq!(clean(&"x".repeat(100), 10), "xxxxxxxxxx...");
        assert_eq!(
            amount("Item $5.00\nTotal: $12.50\nship").as_deref(),
            Some("Total: $12.50")
        );
        assert_eq!(amount("12.000 ₫").as_deref(), Some("12.000 ₫"));
        assert_eq!(amount("no price here"), None);
        let (a, mut b) = (
            p("https://a.example/checkout?x=1", "Pay", true),
            p("https://a.example/checkout?x=2", "Pay", true),
        );
        assert!(same_target(&a, &b));
        b.label = "Cancel".into();
        assert!(!same_target(&a, &b));
    }

    #[test]
    fn invisible_characters_hash_routes_and_form_submit_do_not_hide_a_hit() {
        let shop = "https://shop.example/x";
        assert!(
            assess(
                &p(shop, "Pl\u{200b}ace\u{ad} or\u{feff}der", true),
                Kind::Click
            )
            .is_some()
        );
        assert!(page_hit("https://shop.example/#/checkout/pay"));
        assert!(page_hit("https://shop.example/app?step=payment"));
        assert!(!page_hit("https://shop.example/app?step=browse#top"));
        // Enter in a field of a form whose submit button pays
        let mut f = p(shop, "Email", true);
        assert!(assess(&f, Kind::Key).is_none());
        f.submit_label = "Place order".into();
        assert!(assess(&f, Kind::Key).is_some());
    }
}
