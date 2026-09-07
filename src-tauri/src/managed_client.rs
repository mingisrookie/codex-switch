use serde::{Deserialize, Serialize};

pub const TRUSTED_CHATGPT_AUMIDS: &[&str] = &[
    "OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0!ChatGPT",
    "OpenAI.Codex_2p2nqsd0c76g0!App",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedClientPackage {
    pub aumid: String,
    pub package_name: Option<String>,
    pub package_family_name: Option<String>,
    pub version: Option<String>,
}

#[cfg(windows)]
pub fn inspect_managed_client_packages() -> Vec<ManagedClientPackage> {
    use windows::{
        core::HSTRING,
        ApplicationModel::{AppInfo, PackageVersion},
    };

    fn package_version(value: PackageVersion) -> String {
        format!(
            "{}.{}.{}.{}",
            value.Major, value.Minor, value.Build, value.Revision
        )
    }

    TRUSTED_CHATGPT_AUMIDS
        .iter()
        .filter_map(|aumid| {
            let app_info = AppInfo::GetFromAppUserModelId(&HSTRING::from(*aumid)).ok()?;
            let package_family_name = app_info
                .PackageFamilyName()
                .ok()
                .map(|value| value.to_string())
                .filter(|value| !value.is_empty());
            let package = app_info.Package().ok();
            let package_id = package.as_ref().and_then(|package| package.Id().ok());
            let package_name = package_id
                .as_ref()
                .and_then(|id| id.Name().ok())
                .map(|value| value.to_string())
                .filter(|value| !value.is_empty());
            let version = package_id
                .as_ref()
                .and_then(|id| id.Version().ok())
                .map(package_version);
            Some(ManagedClientPackage {
                aumid: (*aumid).to_string(),
                package_name,
                package_family_name,
                version,
            })
        })
        .collect()
}

#[cfg(not(windows))]
pub fn inspect_managed_client_packages() -> Vec<ManagedClientPackage> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::TRUSTED_CHATGPT_AUMIDS;

    #[test]
    fn managed_client_allowlist_is_small_and_unique() {
        assert_eq!(TRUSTED_CHATGPT_AUMIDS.len(), 2);
        assert_ne!(TRUSTED_CHATGPT_AUMIDS[0], TRUSTED_CHATGPT_AUMIDS[1]);
        assert!(TRUSTED_CHATGPT_AUMIDS
            .iter()
            .all(|value| value.starts_with("OpenAI.") && value.contains('!')));
    }
}
