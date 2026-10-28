use dudes_proto::Spend;
use dudes_proto::rpc::floor::{SettingsSwitches, SettingsView, SettingsWorkspace};
use dudes_providers::registry::Adapters;
use dudes_types::{Provider, Usd};

use crate::floor_actions::{Enrollment, valid_workspace_id};

pub(crate) fn view(
    switches: SettingsSwitches,
    folder_picker: bool,
    create_parent: Option<String>,
    floor_usd_per_day: Option<Usd>,
    reserve: Spend,
    enrollment: &Enrollment,
    adapters: &Adapters,
) -> Option<SettingsView> {
    let version = env!("CARGO_PKG_VERSION");
    if enrollment.profiles.len() > 32 || version.len() > 128 {
        return None;
    }
    let mut workspaces = Vec::with_capacity(enrollment.profiles.len());
    for profile in &enrollment.profiles {
        if !valid_workspace_id(&profile.id)
            || profile.label.is_empty()
            || profile.label.len() > 128
            || profile.label.chars().any(char::is_control)
        {
            return None;
        }
        let mut row = SettingsWorkspace::default();
        row.workspace_id.clone_from(&profile.id);
        row.label.clone_from(&profile.label);
        row.agent_usd_per_day_micros = profile
            .agent_budget
            .usd_per_day
            .map(|value| value.micros().to_string());
        row.agent_tokens_per_day = profile
            .agent_budget
            .tokens_per_day
            .map(|value| value.count().to_string());
        row.goal_usd_micros = profile
            .goal_budget
            .usd
            .map(|value| value.micros().to_string());
        row.goal_tokens = profile
            .goal_budget
            .tokens
            .map(|value| value.count().to_string());
        workspaces.push(row);
    }
    workspaces.sort_by(|a, b| a.workspace_id.cmp(&b.workspace_id));
    if workspaces
        .windows(2)
        .any(|rows| rows[0].workspace_id == rows[1].workspace_id)
    {
        return None;
    }
    let mut report = SettingsView::default();
    report.switches = switches;
    report.folder_picker = folder_picker;
    report.create_parent = create_parent;
    report.budget.floor_usd_per_day_micros =
        floor_usd_per_day.map(|value| value.micros().to_string());
    report.budget.per_turn_usd_micros = reserve.usd.micros().to_string();
    report.budget.per_turn_tokens = reserve.tokens.count().to_string();
    report.providers = Provider::ALL
        .into_iter()
        .filter(|provider| adapters.get(*provider).is_some())
        .collect();
    report.workspaces = workspaces;
    report.about.daemon_version = version.into();
    report.about.browser_wire_version = crate::floor_gateway::BROWSER_WIRE_VERSION;
    Some(report)
}

/// The wire bytes of `view`, or `None` over the 32 KiB bound.
pub(crate) fn serialize(view: &SettingsView) -> Option<Vec<u8>> {
    serde_json::to_vec(view)
        .ok()
        .filter(|bytes| bytes.len() <= 32_768)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_says_whether_the_folder_dialog_is_available() {
        for available in [true, false] {
            let view = view(
                SettingsSwitches::default(),
                available,
                None,
                None,
                Spend::DEFAULT_PER_TURN_FLOOR,
                &Enrollment::default(),
                &Adapters::default(),
            )
            .expect("a view");
            let wire: serde_json::Value =
                serde_json::from_slice(&serialize(&view).unwrap()).unwrap();
            assert_eq!(wire["folder_picker"], available);
        }
    }

    #[test]
    fn a_settings_view_serializes_up_to_32_kib_and_no_further() {
        let sized = |len| {
            let mut view = SettingsView::default();
            view.about.daemon_version = "v".repeat(len);
            view
        };
        let open = serialize(&sized(0)).expect("an empty view fits").len();
        assert_eq!(
            serialize(&sized(32_768 - open)).map(|bytes| bytes.len()),
            Some(32_768)
        );
        assert_eq!(serialize(&sized(32_768 - open + 1)), None);
    }
}
