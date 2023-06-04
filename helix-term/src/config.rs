use crate::keymap::{self, MappableCommand};
use crate::keymap::{merge_keys, Keymap};
use helix_loader::merge_toml_values;
use helix_view::document::Mode;
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Display;
use std::fs;
use std::io::Error as IOError;
use toml::de::Error as TomlError;

#[derive(Default, Debug, Clone, PartialEq)]
pub struct Config {
    pub theme: Option<String>,
    pub icons: Option<String>,
    pub keys: KeymapConfig,
    pub editor: helix_view::editor::Config,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRaw {
    pub theme: Option<String>,
    pub icons: Option<String>,
    pub keys: Option<KeymapConfig>,
    pub editor: Option<toml::Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct KeymapConfig {
    /// An alternative command to run when tab is pressed and the cursor has
    /// text other than whitespace to its left on the current line.
    pub supertab: Option<MappableCommand>,

    #[serde(flatten)]
    pub bindings: HashMap<Mode, Keymap>,
}

impl Default for KeymapConfig {
    fn default() -> KeymapConfig {
        KeymapConfig {
            supertab: None,
            bindings: keymap::default(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigLoadError {
    BadConfig(TomlError),
    Error(IOError),
}

impl Default for ConfigLoadError {
    fn default() -> Self {
        ConfigLoadError::Error(IOError::new(std::io::ErrorKind::NotFound, "place holder"))
    }
}

impl Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigLoadError::BadConfig(err) => err.fmt(f),
            ConfigLoadError::Error(err) => err.fmt(f),
        }
    }
}

impl Config {
    pub fn load(
        global: Result<String, ConfigLoadError>,
        local: Result<String, ConfigLoadError>,
    ) -> Result<Config, ConfigLoadError> {
        let global_config: Result<ConfigRaw, ConfigLoadError> =
            global.and_then(|file| toml::from_str(&file).map_err(ConfigLoadError::BadConfig));

        let local_config: Result<ConfigRaw, ConfigLoadError> =
            local.and_then(|file| toml::from_str(&file).map_err(ConfigLoadError::BadConfig));

        let mut result_keymap_config = KeymapConfig::default();

        let mut merge_keymap_configs = |config: &ConfigRaw| {
            let result_keymap_config = &mut result_keymap_config;

            if let Some(ref keymap_config) = config.keys {
                if let Some(supertab_config) = &keymap_config.supertab {
                    result_keymap_config.supertab = Some(supertab_config.clone());
                }

                merge_keys(
                    &mut result_keymap_config.bindings,
                    keymap_config.bindings.clone(),
                )
            }
        };

        let res = match (global_config, local_config) {
            (Ok(global), Ok(local)) => {
                merge_keymap_configs(&global);
                merge_keymap_configs(&local);

                let editor = match (global.editor, local.editor) {
                    (None, None) => helix_view::editor::Config::default(),
                    (None, Some(val)) | (Some(val), None) => {
                        val.try_into().map_err(ConfigLoadError::BadConfig)?
                    }
                    (Some(global), Some(local)) => merge_toml_values(global, local, 3)
                        .try_into()
                        .map_err(ConfigLoadError::BadConfig)?,
                };

                Config {
                    theme: local.theme.or(global.theme),
                    icons: local.icons.or(global.icons),
                    keys: result_keymap_config,
                    editor,
                }
            }
            // if any configs are invalid return that first
            (_, Err(ConfigLoadError::BadConfig(err)))
            | (Err(ConfigLoadError::BadConfig(err)), _) => {
                return Err(ConfigLoadError::BadConfig(err))
            }
            (Ok(config), Err(_)) | (Err(_), Ok(config)) => {
                merge_keymap_configs(&config);

                Config {
                    theme: config.theme,
                    icons: config.icons,
                    keys: result_keymap_config,
                    editor: config.editor.map_or_else(
                        || Ok(helix_view::editor::Config::default()),
                        |val| val.try_into().map_err(ConfigLoadError::BadConfig),
                    )?,
                }
            }

            // these are just two io errors return the one for the global config
            (Err(err), Err(_)) => return Err(err),
        };

        Ok(res)
    }

    pub fn load_default() -> Result<Config, ConfigLoadError> {
        let global_config =
            fs::read_to_string(helix_loader::config_file()).map_err(ConfigLoadError::Error);
        let local_config = fs::read_to_string(helix_loader::workspace_config_file())
            .map_err(ConfigLoadError::Error);
        Config::load(global_config, local_config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Config {
        fn load_test(config: &str) -> Config {
            Config::load(Ok(config.to_owned()), Err(ConfigLoadError::default())).unwrap()
        }
    }

    #[test]
    fn parsing_keymaps_config_file() {
        use crate::keymap;
        use crate::keymap::Keymap;
        use helix_core::hashmap;
        use helix_view::document::Mode;

        let sample_keymaps = r#"
            [keys.insert]
            y = "move_line_down"
            S-C-a = "delete_selection"

            [keys.normal]
            A-F12 = "move_next_word_end"
        "#;

        let mut keymap_config = KeymapConfig::default();

        merge_keys(
            &mut keymap_config.bindings,
            hashmap! {
                Mode::Insert => Keymap::new(keymap!({ "Insert mode"
                    "y" => move_line_down,
                    "S-C-a" => delete_selection,
                })),
                Mode::Normal => Keymap::new(keymap!({ "Normal mode"
                    "A-F12" => move_next_word_end,
                })),
            },
        );

        assert_eq!(
            Config::load_test(sample_keymaps),
            Config {
                keys: keymap_config,
                ..Default::default()
            }
        );
    }

    #[test]
    fn keys_resolve_to_correct_defaults() {
        // From serde default
        let default_keys = Config::load_test("").keys.bindings;
        assert_eq!(default_keys, keymap::default());

        // From the Default trait
        let default_keys = Config::default().keys.bindings;
        assert_eq!(default_keys, keymap::default());
    }
}
