//! Each status, its reason, and the home page's choice, from the facts alone.

use meridian_domain::v1::{
    DatasetLiveness, DatasetRef, ListDatasetsReply, PluginLaunch, PluginLaunchState, PluginReport,
};

use super::*;

const NOW: i64 = 1_790_380_800_000_000_000;
const S: i64 = 1_000_000_000;

fn launch(state: PluginLaunchState, launched_ago_s: i64) -> PluginLaunch {
    PluginLaunch {
        instance_id: "alpaca-1".into(),
        state: state as i32,
        launched_at_ns: NOW - launched_ago_s * S,
        stopped_by: "https://directory.example.org|ada".into(),
        stopped_at_ns: NOW - 3_600 * S,
        ..Default::default()
    }
}

fn report(registered: bool, healthy: bool, detail: &str, ago_s: i64) -> PluginReport {
    PluginReport {
        plugin_instance_id: "alpaca-1".into(),
        registered,
        healthy,
        health_detail: detail.into(),
        reported_at_ns: NOW - ago_s * S,
        ..Default::default()
    }
}

fn listed(alive_ago_s: Option<i64>) -> ListDatasetsReply {
    ListDatasetsReply {
        datasets: vec![DatasetRef {
            dataset: "alpaca-1:bars".into(),
            instance: "alpaca-1".into(),
            // A ceiling long past: no staleness is judged here.
            stale_after_ns: 60 * S,
            liveness: alive_ago_s
                .map(|ago| DatasetLiveness {
                    dataset: "alpaca-1:bars".into(),
                    alive_at_ns: NOW - ago * S,
                    ..Default::default()
                })
                .into_iter()
                .collect(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn step(words: &str, done: Option<bool>, live: bool) -> Step {
    Step {
        words: words.into(),
        done,
        href: None,
        deployment_admins: true,
        live,
    }
}

struct Facts {
    launch: Option<PluginLaunch>,
    report: Option<PluginReport>,
    steps: Vec<Step>,
    listed: Option<ListDatasetsReply>,
    missing: Vec<String>,
    unlinked: usize,
    trading: Option<&'static str>,
}

fn working_facts() -> Facts {
    Facts {
        launch: Some(launch(PluginLaunchState::Launched, 3_600)),
        report: Some(report(true, true, "", 5)),
        steps: vec![
            step("Enter the key", Some(true), false),
            step("Check the connection", Some(true), true),
            step("License and entitle its datasets", Some(true), false),
            step("See data arrive", None, true),
        ],
        listed: Some(listed(Some(20))),
        missing: vec![],
        unlinked: 0,
        trading: None,
    }
}

fn judged(facts: &Facts) -> Status {
    status(&Known {
        instance: "alpaca-1",
        launch: facts.launch.as_ref(),
        report: facts.report.as_ref(),
        steps: &facts.steps,
        listed: facts.listed.as_ref(),
        stopped_by: "Ada Lovelace",
        missing: &facts.missing,
        unlinked: facts.unlinked,
        trading: facts.trading,
        now: NOW,
    })
}

#[test]
fn healthy_and_set_up_is_working_its_note_the_age_of_its_data_however_old() {
    let mut facts = working_facts();
    let said = judged(&facts);
    assert_eq!(said.standing, Standing::Working);
    assert_eq!(said.reason, "Last data 20 s ago.");
    assert!(said.on_home());
    // Past the dataset's stale ceiling: still Working, the age said.
    facts.listed = Some(listed(Some(3 * 3_600)));
    let said = judged(&facts);
    assert_eq!(said.standing, Standing::Working, "no staleness amber");
    assert_eq!(said.reason, "Last data 3 h ago.");
    // Bringing in no data: its last report.
    facts.listed = None;
    assert_eq!(judged(&facts).reason, "Last report 5 s ago.");
}

#[test]
fn a_step_from_the_records_open_is_setting_up_and_off_the_home_page() {
    let mut facts = working_facts();
    facts.steps[2].done = Some(false);
    let said = judged(&facts);
    assert_eq!(said.standing, Standing::SettingUp);
    assert_eq!(said.reason, "Next step: license and entitle its datasets.");
    assert!(!said.on_home());
    assert_eq!(badge(&said).contains("state=\"warn\""), true);
}

#[test]
fn a_live_step_open_never_holds_it_setting_up() {
    // Not connected: a live step, open, on a healthy plugin is something it
    // needs of a person, and it stays on the home page.
    let mut facts = working_facts();
    facts.steps = vec![
        step("Enter the credentials", Some(true), false),
        step("Connect", Some(false), true),
        step("Have accounts granted", Some(true), false),
        step("Link accounts", Some(false), true),
    ];
    let said = judged(&facts);
    assert_eq!(said.standing, Standing::Working);
    assert_eq!(said.needs_you, ["Connect", "Link accounts"]);
    assert!(said.on_home());
    // An undecided step (None) is not open.
    let mut facts = working_facts();
    facts.listed = Some(listed(None));
    assert_eq!(judged(&facts).standing, Standing::Working);
    // The connection's own check, open, is the plugin not healthy.
    let mut facts = working_facts();
    facts.steps[1].done = Some(false);
    facts.report = Some(report(true, false, "Tradier refused the token (401).", 5));
    let said = judged(&facts);
    assert_eq!(said.standing, Standing::NotHealthy);
    assert_eq!(said.reason, "Tradier refused the token (401).");
    assert_eq!(said.at_label, "Reported");
    assert!(said.needs_you.is_empty());
    assert!(said.on_home(), "not healthy stays on the home page");
}

#[test]
fn not_heard_from_or_not_registered_is_starting_within_silent_of_launch_then_not_healthy() {
    for report in [None, Some(report(false, false, "", 5))] {
        let mut facts = working_facts();
        facts.report = report.clone();
        facts.launch = Some(launch(PluginLaunchState::Launched, 30));
        let said = judged(&facts);
        assert_eq!(said.standing, Standing::Starting, "{report:?}");
        assert_eq!(said.reason, "Waiting for its first report.");
        assert!(said.on_home(), "starting counts as set up");
        assert!(badge(&said).contains("state=\"busy\""));
        facts.launch = Some(launch(PluginLaunchState::Launched, 91));
        let said = judged(&facts);
        assert_eq!(said.standing, Standing::NotHealthy, "{report:?}");
    }
    // Silent: not healthy, whenever it was launched.
    let mut facts = working_facts();
    facts.launch = Some(launch(PluginLaunchState::Launched, 30));
    facts.report = Some(report(true, true, "", 91));
    let said = judged(&facts);
    assert_eq!(said.standing, Standing::NotHealthy);
    assert_eq!(said.at_label, "Last report");
}

#[test]
fn stopped_says_by_whom_and_when_and_wins_over_everything() {
    let mut facts = working_facts();
    facts.launch = Some(launch(PluginLaunchState::Stopped, 7_200));
    facts.steps[0].done = Some(false);
    let said = judged(&facts);
    assert_eq!(said.standing, Standing::Stopped);
    assert_eq!(said.reason, "Stopped by Ada Lovelace.");
    assert_eq!(said.at_ns, NOW - 3_600 * S);
    assert!(!said.on_home());
    let drawn = badge(&said);
    assert!(!drawn.contains("state="), "stopped is no state: {drawn}");
    assert!(drawn.contains("at-label=\"Stopped\""));
    facts.launch = None;
    facts.report = None;
    assert_eq!(judged(&facts).reason, "Not launched.");
}

#[test]
fn needs_you_carries_missing_settings_and_unlinked_accounts_into_the_note() {
    let mut facts = working_facts();
    facts.missing = vec!["Client ID".into(), "Consumer key".into()];
    facts.unlinked = 2;
    facts.trading = Some("Demo");
    let said = judged(&facts);
    assert_eq!(
        said.needs_you,
        [
            "Settings: needs Client ID, Consumer key",
            "2 external accounts not linked"
        ]
    );
    assert!(badge(&said).contains(
        "detail=\"Last data 20 s ago. Needs you: Settings: needs Client ID, Consumer key. \
         Needs you: 2 external accounts not linked. Trading: Demo.\""
    ));
    let json = said.as_json(|words| words.to_string());
    assert_eq!(json["status"], "working");
    assert_eq!(json["trading"], "Demo");
    assert_eq!(json["on_home"], true);
    assert_eq!(json["needs_you"][1], "2 external accounts not linked");
}

#[test]
fn a_plugins_words_are_escaped_in_the_badge() {
    let mut facts = working_facts();
    facts.report = Some(report(true, false, "<b>bad</b> \"token\"", 5));
    let drawn = badge(&judged(&facts));
    assert!(
        drawn.contains("detail=\"&lt;b&gt;bad&lt;/b&gt; &quot;token&quot;\""),
        "{drawn}"
    );
}

#[test]
fn each_standing_round_trips_its_name() {
    for standing in Standing::ALL {
        assert_eq!(Standing::named(standing.name()), Some(standing));
    }
}
