//! Companies (contract v28, W3.32; ruling 612): resolved by record or by
//! open identifiers as of a date, a miss asked of the platform through the
//! conductor and its answer kept; queued or unreachable, nothing kept.

use std::sync::Mutex;

use super::*;
use crate::MemoryStore;
use meridian_domain::v1::{FormerName, LicenceList};

const NOW: i64 = 1_792_300_000_000_000_000;
const APPLE: &str = "ENT-01JC28APPLEINC000000000001";
const DAY: i64 = 86_400_000_000_000;

/// A platform that answers as told, and remembers what it was asked.
#[derive(Default)]
struct Platform {
    answers: Mutex<Vec<Asked>>,
    asked: Mutex<Vec<MatchOrMintCompanyRequest>>,
}

impl Platform {
    fn answering(answers: Vec<Asked>) -> Self {
        Platform {
            answers: Mutex::new(answers),
            asked: Mutex::default(),
        }
    }

    fn asked(&self) -> Vec<MatchOrMintCompanyRequest> {
        self.asked.lock().unwrap().clone()
    }
}

impl AskCompany for Platform {
    fn ask(&self, request: MatchOrMintCompanyRequest) -> Asked {
        self.asked.lock().unwrap().push(request);
        let mut answers = self.answers.lock().unwrap();
        if answers.is_empty() {
            Asked::Unreachable("not answering".into())
        } else {
            answers.remove(0)
        }
    }
}

fn id(scheme: &str, value: &str) -> Identifier {
    Identifier {
        scheme: scheme.into(),
        value: value.into(),
        ..Default::default()
    }
}

fn apple(version: i64) -> CompanyRecord {
    CompanyRecord {
        company_id: APPLE.into(),
        name: "Apple Inc.".into(),
        identifiers: vec![id("cik", "0000320193"), id("lei", "HWUPKR0MPOU8FGXBT394")],
        former_names: vec![FormerName {
            name: "APPLE COMPUTER INC".into(),
            valid_from: String::new(),
            valid_until: "2007-01-04".into(),
        }],
        version,
    }
}

fn answered(company: CompanyRecord, minted: bool) -> Asked {
    Asked::Answered(MatchOrMintCompanyReply {
        company: Some(company),
        minted,
        pending_review: false,
    })
}

/// A store whose licence list admits the SEC's CIK, as the platform's does
/// (W1.32, W3.25).
fn listing_cik() -> MemoryStore {
    let store = MemoryStore::new();
    let mut list: LicenceList = crate::one_id::default_licence();
    list.version = 1;
    if !list.schemes.iter().any(|scheme| scheme == "cik") {
        list.schemes.push("cik".into());
    }
    store.keep_licence_list(&list, NOW).unwrap();
    store
}

fn by_cik(cik: &str) -> ResolveCompanyRequest {
    ResolveCompanyRequest {
        identifiers: vec![id("cik", cik)],
        ..Default::default()
    }
}

#[test]
fn a_miss_asks_the_platform_and_keeps_its_answer() {
    let store = listing_cik();
    let platform = Platform::answering(vec![answered(apple(1), true)]);
    let resolved = resolve(&store, &platform, &by_cik("0000320193"), NOW)
        .unwrap()
        .unwrap();
    assert_eq!(resolved.company, apple(1));
    assert!(resolved.kept);
    let asked = platform.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].identifiers, vec![id("cik", "0000320193")]);
    assert!(asked[0].name.is_empty(), "a resolve carries no name");
    assert!(!asked[0].request_key.is_empty());

    // Held now: answered by the store, nothing asked again, and the LEI the
    // record carries finds it too.
    let again = resolve(&store, &platform, &by_cik("0000320193"), NOW)
        .unwrap()
        .unwrap();
    assert!(!again.kept);
    let by_lei = ResolveCompanyRequest {
        identifiers: vec![id("lei", "HWUPKR0MPOU8FGXBT394")],
        ..Default::default()
    };
    assert_eq!(
        resolve(&store, &platform, &by_lei, NOW)
            .unwrap()
            .unwrap()
            .company,
        apple(1)
    );
    assert_eq!(platform.asked().len(), 1);
}

#[test]
fn by_company_id_it_answers_the_latest_version_and_never_asks() {
    let store = listing_cik();
    store.keep_company(&apple(1), NOW).unwrap();
    let mut renamed = apple(2);
    renamed.name = "Apple Inc. (2)".into();
    store.keep_company(&renamed, NOW).unwrap();
    let platform = Platform::default();
    let asking = ResolveCompanyRequest {
        company_id: APPLE.into(),
        ..Default::default()
    };
    let resolved = resolve(&store, &platform, &asking, NOW).unwrap().unwrap();
    assert_eq!(resolved.company.version, 2);

    let missing = ResolveCompanyRequest {
        company_id: "ENT-01JC28NOTHELD0000000000001".into(),
        ..Default::default()
    };
    let refused = resolve(&store, &platform, &missing, NOW)
        .unwrap()
        .unwrap_err();
    assert_eq!(refused.fields, vec!["company_id".to_string()]);
    assert!(platform.asked().is_empty(), "nothing to look for by an ID");
}

#[test]
fn queued_or_unreachable_is_a_miss_with_nothing_kept_and_the_next_miss_asks_again() {
    let store = listing_cik();
    let platform = Platform::answering(vec![Asked::Answered(MatchOrMintCompanyReply {
        company: None,
        minted: false,
        pending_review: true,
    })]);
    let queued = resolve(&store, &platform, &by_cik("0001067983"), NOW)
        .unwrap()
        .unwrap_err();
    assert_eq!(queued.fields, vec!["identifiers".to_string()]);
    assert!(queued.words.contains("queued"), "{}", queued.words);
    // Unreachable (the answers ran out): a miss again, asked with a new key.
    let away = resolve(&store, &platform, &by_cik("0001067983"), NOW)
        .unwrap()
        .unwrap_err();
    assert!(
        away.words.contains("could not be reached"),
        "{}",
        away.words
    );
    let asked = platform.asked();
    assert_eq!(asked.len(), 2);
    assert_ne!(asked[0].request_key, asked[1].request_key);
    assert!(store
        .companies_with("cik", "0001067983")
        .unwrap()
        .is_empty());
}

#[test]
fn only_schemes_on_the_licence_list_are_asked_by() {
    // No list read: the open schemes, the LEI and from C2 the CIK among
    // them (W3.25), so a company is asked about by its CIK.
    let unread = MemoryStore::new();
    let asking = Platform::answering(vec![answered(apple(1), true)]);
    resolve(&unread, &asking, &by_cik("0000320193"), NOW)
        .unwrap()
        .unwrap();
    assert_eq!(asking.asked()[0].identifiers, vec![id("cik", "0000320193")]);

    // A list read that names no CIK: never sent by it.
    let store = MemoryStore::new();
    let mut list: LicenceList = crate::one_id::default_licence();
    list.version = 1;
    list.schemes.retain(|scheme| scheme != "cik");
    store.keep_licence_list(&list, NOW).unwrap();
    let platform = Platform::default();
    let refused = resolve(&store, &platform, &by_cik("0000320193"), NOW)
        .unwrap()
        .unwrap_err();
    assert!(refused.words.contains("licence list"), "{}", refused.words);
    assert!(platform.asked().is_empty());

    let both = ResolveCompanyRequest {
        identifiers: vec![id("cik", "0000320193"), id("lei", "HWUPKR0MPOU8FGXBT394")],
        ..Default::default()
    };
    let _ = resolve(&store, &platform, &both, NOW).unwrap();
    let asked = platform.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].identifiers,
        vec![id("lei", "HWUPKR0MPOU8FGXBT394")]
    );
}

#[test]
fn an_answer_not_under_the_platforms_prefix_is_never_kept() {
    let store = listing_cik();
    let mut local = apple(1);
    local.company_id = "LCL-01JC28APPLEINC000000000001".into();
    let platform = Platform::answering(vec![answered(local, true)]);
    assert!(resolve(&store, &platform, &by_cik("0000320193"), NOW)
        .unwrap()
        .is_err());
    assert!(store
        .companies_with("cik", "0000320193")
        .unwrap()
        .is_empty());
}

#[test]
fn identifiers_resolve_as_of_a_date_against_their_windows() {
    let store = listing_cik();
    // A CIK the company held until a day ago, and one it holds since.
    let mut moved = apple(1);
    moved.identifiers = vec![
        Identifier {
            valid_from_ns: NOW - 10 * DAY,
            valid_until_ns: NOW - DAY,
            ..id("cik", "0000000001")
        },
        Identifier {
            valid_from_ns: NOW - DAY,
            ..id("cik", "0000320193")
        },
    ];
    store.keep_company(&moved, NOW).unwrap();
    let platform = Platform::default();
    let then = ResolveCompanyRequest {
        identifiers: vec![id("cik", "0000000001")],
        as_of_ns: NOW - 5 * DAY,
        ..Default::default()
    };
    assert_eq!(
        resolve(&store, &platform, &then, NOW)
            .unwrap()
            .unwrap()
            .company
            .company_id,
        APPLE
    );
    // Now the old CIK names nothing held; the platform is asked about it.
    assert!(resolve(&store, &platform, &by_cik("0000000001"), NOW)
        .unwrap()
        .is_err());
    assert_eq!(platform.asked().len(), 1);
}

#[test]
fn two_companies_named_at_once_are_refused() {
    let store = listing_cik();
    store.keep_company(&apple(1), NOW).unwrap();
    let other = CompanyRecord {
        company_id: "ENT-01JC28BERKSHIRE00000000001".into(),
        name: "Berkshire Hathaway Inc".into(),
        identifiers: vec![id("cik", "0001067983")],
        former_names: Vec::new(),
        version: 1,
    };
    store.keep_company(&other, NOW).unwrap();
    let both = ResolveCompanyRequest {
        identifiers: vec![id("cik", "0000320193"), id("cik", "0001067983")],
        ..Default::default()
    };
    let refused = resolve(&store, &Platform::default(), &both, NOW)
        .unwrap()
        .unwrap_err();
    assert!(refused.words.contains("different companies"));
}

#[test]
fn a_request_naming_nothing_is_refused() {
    let store = MemoryStore::new();
    let refused = resolve(
        &store,
        &Platform::default(),
        &ResolveCompanyRequest::default(),
        NOW,
    )
    .unwrap()
    .unwrap_err();
    assert_eq!(refused.fields, vec!["identifiers".to_string()]);
}
