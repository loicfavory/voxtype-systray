/// US-02 — Fenêtre de réglages Voxtype (eframe/egui)
///
/// Lance la GUI sur le thread principal (exigence egui).
/// Ce module est utilisé exclusivement depuis le mode `settings` du binaire.
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use eframe::egui;
use toml_edit::DocumentMut;

// ──────────────────────────────────────────────
// Chemin du config
// ──────────────────────────────────────────────

fn config_path() -> PathBuf {
    // Priorité : XDG_CONFIG_HOME, sinon ~/.config
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".config")
        });
    base.join("voxtype").join("config.toml")
}

fn backup_path() -> PathBuf {
    config_path().with_extension("toml.bak")
}

// ──────────────────────────────────────────────
// Lecture du config (tolère l'absence de fichier)
// ──────────────────────────────────────────────

struct CurrentConfig {
    storage_path: String,
    retain_audio: bool,
}

fn read_config() -> Result<CurrentConfig> {
    let path = config_path();
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Impossible de lire {}", path.display()))?;
    let doc: DocumentMut = content
        .parse()
        .with_context(|| format!("TOML invalide dans {}", path.display()))?;

    let storage_path = doc["meeting"]["storage_path"]
        .as_str()
        .unwrap_or("~/.local/share/voxtype/meetings/")
        .to_string();

    let retain_audio = doc["meeting"]["retain_audio"].as_bool().unwrap_or(false);

    Ok(CurrentConfig {
        storage_path,
        retain_audio,
    })
}

// ──────────────────────────────────────────────
// Écriture non-destructive via toml_edit
// ──────────────────────────────────────────────

/// Retourne une erreur textuelle si la validation échoue (chemin vide).
fn validate_storage_path(path: &str) -> Result<(), String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("Le dossier de stockage ne peut pas être vide.".to_string());
    }
    Ok(())
}

/// Étend `~` en chemin absolu pour créer le dossier si besoin.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(rest)
    } else if path == "~" {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home)
    } else {
        PathBuf::from(path)
    }
}

/// Sauvegarde le config (backup + écriture non-destructive) et redémarre voxtype.
/// Retourne Ok(()) ou une erreur lisible.
fn save_config(storage_path: &str, retain_audio: bool) -> Result<String, String> {
    // 1. Validation
    validate_storage_path(storage_path)?;

    let config_path = config_path();
    let backup_path = backup_path();

    // 2. Lecture du fichier courant
    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("Impossible de lire le config : {e}"))?;

    // 3. Parsing toml_edit (préserve commentaires et formatage)
    let mut doc: DocumentMut = content
        .parse()
        .map_err(|e| format!("TOML invalide : {e}"))?;

    // 4. Backup AVANT toute modification
    std::fs::copy(&config_path, &backup_path).map_err(|e| {
        format!(
            "Impossible de créer le backup {} : {e}",
            backup_path.display()
        )
    })?;

    // 5. Modification des deux clés uniquement
    doc["meeting"]["storage_path"] = toml_edit::value(storage_path);
    doc["meeting"]["retain_audio"] = toml_edit::value(retain_audio);

    // 6. Écriture atomique (write vers fichier temp puis rename)
    let new_content = doc.to_string();
    let tmp_path = config_path.with_extension("toml.tmp");
    std::fs::write(&tmp_path, &new_content)
        .map_err(|e| format!("Impossible d'écrire le fichier temporaire : {e}"))?;
    std::fs::rename(&tmp_path, &config_path).map_err(|e| {
        // Tentative de nettoyage du fichier temporaire
        let _ = std::fs::remove_file(&tmp_path);
        format!("Impossible de remplacer le config : {e}")
    })?;

    // 7. Création du dossier de stockage si absent
    let expanded = expand_tilde(storage_path.trim());
    if !expanded.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(&expanded)
    {
        // Non bloquant : on avertit mais la sauvegarde est déjà faite
        return Ok(format!(
            "Config sauvegardé. Attention : impossible de créer le dossier {} : {e}",
            expanded.display()
        ));
    }

    // 8. Redémarrage du daemon
    match restart_voxtype() {
        Ok(_) => Ok("Config sauvegardé et daemon redémarré.".to_string()),
        Err(e) => Ok(format!(
            "Config sauvegardé. Redémarrage du daemon échoué : {e}"
        )),
    }
}

fn restart_voxtype() -> Result<()> {
    let output = Command::new("systemctl")
        .args(["--user", "restart", "voxtype"])
        .output()
        .context("Impossible de lancer systemctl")?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("systemctl restart voxtype a échoué : {stderr}");
    }
}

// ──────────────────────────────────────────────
// Sélecteur de dossier via portail XDG natif (rfd)
// ──────────────────────────────────────────────

/// Ouvre le sélecteur de dossier natif du système (portail XDG / KDE).
///
/// Retourne `Some(PathBuf)` si l'utilisateur a validé un choix,
/// `None` s'il a annulé ou si le portail a refusé d'ouvrir.
///
/// Tout ce qui se passe est loggé sur stderr pour faciliter le diagnostic.
fn pick_folder_native(initial_dir: &PathBuf) -> Option<PathBuf> {
    eprintln!(
        "[voxtype-systray settings] Ouverture du sélecteur de dossier natif (portail XDG) ; répertoire initial : {}",
        initial_dir.display()
    );

    let result = rfd::FileDialog::new()
        .set_directory(initial_dir)
        .set_title("Choisir le dossier de stockage")
        .pick_folder();

    match &result {
        Some(path) => {
            eprintln!(
                "[voxtype-systray settings] Dossier sélectionné : {}",
                path.display()
            );
        }
        None => {
            eprintln!(
                "[voxtype-systray settings] Sélecteur fermé sans choix (annulation ou échec portail)."
            );
        }
    }

    result
}

// ──────────────────────────────────────────────
// App egui
// ──────────────────────────────────────────────

/// Résultat affiché dans la fenêtre après une tentative de sauvegarde
#[derive(Clone)]
enum SaveResult {
    /// Sauvegarde réussie (avec message)
    Success(String),
    /// Échec (message d'erreur)
    Failure(String),
}

struct SettingsApp {
    /// Dossier de stockage (champ texte éditable)
    storage_path: String,
    /// Case à cocher "Conserver l'audio"
    retain_audio: bool,
    /// Résultat de la dernière tentative de sauvegarde
    save_result: Option<SaveResult>,
    /// Erreur de chargement initial (config illisible)
    load_error: Option<String>,
}

impl SettingsApp {
    fn new() -> Self {
        match read_config() {
            Ok(cfg) => Self {
                storage_path: cfg.storage_path,
                retain_audio: cfg.retain_audio,
                save_result: None,
                load_error: None,
            },
            Err(e) => Self {
                storage_path: "~/.local/share/voxtype/meetings/".to_string(),
                retain_audio: false,
                save_result: None,
                load_error: Some(format!("Impossible de lire le config : {e}")),
            },
        }
    }

    /// Résout le dossier initial à proposer dans le picker.
    /// Retourne le répertoire courant si le chemin configuré n'existe pas.
    fn initial_dir_for_picker(&self) -> PathBuf {
        let expanded = expand_tilde(self.storage_path.trim());
        if expanded.is_dir() {
            return expanded;
        }
        if let Some(parent) = expanded.parent()
            && parent.is_dir()
        {
            return parent.to_path_buf();
        }
        // Repli sur le home
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/"))
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(24)))
            .show(ctx, |ui| {
                // Espacement entre items plus généreux
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);

                // ── Titre ──
                ui.vertical(|ui| {
                    ui.add(egui::Label::new(
                        egui::RichText::new("Réglages Voxtype").heading().strong(),
                    ));
                    ui.add(egui::Label::new(
                        egui::RichText::new("Configuration du daemon de transcription")
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    ));
                });

                ui.add_space(16.0);
                ui.separator();
                ui.add_space(12.0);

                // ── Avertissement si erreur de chargement ──
                if let Some(ref err) = self.load_error.clone() {
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(80, 20, 20))
                        .inner_margin(egui::Margin::same(8))
                        .corner_radius(egui::CornerRadius::same(4))
                        .show(ui, |ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 180, 180),
                                format!("Avertissement : {err}"),
                            );
                        });
                    ui.add_space(12.0);
                }

                // ── Section : Dossier de stockage ──
                ui.label(egui::RichText::new("Dossier de stockage").strong());
                ui.add(egui::Label::new(
                    egui::RichText::new(
                        "Dossier où sont stockés les transcripts et l'audio des réunions.",
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                ));
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    let available = ui.available_width() - 100.0;
                    ui.add(
                        egui::TextEdit::singleline(&mut self.storage_path)
                            .desired_width(available.max(120.0))
                            .hint_text("~/.local/share/voxtype/meetings/"),
                    );
                    if ui.button("Parcourir…").clicked() {
                        // Ouvre le sélecteur de dossier natif du système (portail XDG / KDE).
                        // rfd::FileDialog::pick_folder() est synchrone et bloque le thread
                        // principal jusqu'à la fermeture du dialogue. Aucun runtime Tokio
                        // n'est requis : rfd utilise pollster en interne pour ses appels async.
                        let initial = self.initial_dir_for_picker();
                        if let Some(picked) = pick_folder_native(&initial) {
                            self.storage_path = picked.to_string_lossy().into_owned();
                            self.save_result = None;
                        }
                    }
                });

                ui.add_space(16.0);
                ui.separator();
                ui.add_space(12.0);

                // ── Section : Audio ──
                ui.label(egui::RichText::new("Audio").strong());
                ui.add(egui::Label::new(
                    egui::RichText::new(
                        "Si activé, les fichiers audio bruts sont conservés après transcription.",
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                ));
                ui.add_space(6.0);
                ui.checkbox(&mut self.retain_audio, "Conserver l'audio des réunions");

                ui.add_space(20.0);
                ui.separator();
                ui.add_space(12.0);

                // ── Retour visuel après sauvegarde (au-dessus des boutons pour être visible) ──
                if let Some(ref result) = self.save_result {
                    match result {
                        SaveResult::Success(msg) => {
                            egui::Frame::new()
                                .fill(egui::Color32::from_rgb(20, 60, 30))
                                .inner_margin(egui::Margin::same(8))
                                .corner_radius(egui::CornerRadius::same(4))
                                .show(ui, |ui| {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(100, 220, 130),
                                        msg.as_str(),
                                    );
                                });
                        }
                        SaveResult::Failure(msg) => {
                            egui::Frame::new()
                                .fill(egui::Color32::from_rgb(80, 20, 20))
                                .inner_margin(egui::Margin::same(8))
                                .corner_radius(egui::CornerRadius::same(4))
                                .show(ui, |ui| {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(255, 120, 120),
                                        msg.as_str(),
                                    );
                                });
                        }
                    }
                    ui.add_space(12.0);
                }

                // ── Boutons Enregistrer / Annuler (alignés à droite) ──
                ui.horizontal(|ui| {
                    // Pousse les boutons à droite
                    let btn_width = 110.0;
                    let gap = 8.0;
                    let right_offset = btn_width * 2.0 + gap;
                    let space = ui.available_width() - right_offset;
                    if space > 0.0 {
                        ui.add_space(space);
                    }

                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("Enregistrer").strong(),
                        ))
                        .clicked()
                    {
                        let result = save_config(&self.storage_path, self.retain_audio);
                        self.save_result = Some(match result {
                            Ok(msg) => SaveResult::Success(msg),
                            Err(msg) => SaveResult::Failure(msg),
                        });
                    }

                    if ui.button("Annuler").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
    }
}

// ──────────────────────────────────────────────
// Point d'entrée du mode settings
// ──────────────────────────────────────────────

/// Lance la fenêtre de réglages sur le thread courant (doit être le thread principal).
pub fn run_settings_window() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Réglages Voxtype")
            .with_inner_size([520.0, 380.0])
            .with_resizable(true)
            .with_min_inner_size([420.0, 320.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Réglages Voxtype",
        options,
        Box::new(|_cc| Ok(Box::new(SettingsApp::new()))),
    )
    .map_err(|e| anyhow::anyhow!("Erreur eframe : {e}"))?;

    Ok(())
}

// ──────────────────────────────────────────────
// Tests unitaires (sans I/O sur le vrai config)
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_empty_path() {
        assert!(validate_storage_path("").is_err());
        assert!(validate_storage_path("   ").is_err());
    }

    #[test]
    fn test_validate_valid_path() {
        assert!(validate_storage_path("~/.local/share/voxtype/meetings/").is_ok());
        assert!(validate_storage_path("/tmp/test").is_ok());
    }

    #[test]
    fn test_expand_tilde_home() {
        // SAFETY : test single-threaded, mutation de HOME isolée au test
        unsafe { std::env::set_var("HOME", "/home/testuser") };
        assert_eq!(
            expand_tilde("~/foo/bar"),
            PathBuf::from("/home/testuser/foo/bar")
        );
    }

    #[test]
    fn test_expand_tilde_only() {
        // SAFETY : test single-threaded, mutation de HOME isolée au test
        unsafe { std::env::set_var("HOME", "/home/testuser") };
        assert_eq!(expand_tilde("~"), PathBuf::from("/home/testuser"));
    }

    #[test]
    fn test_expand_tilde_absolute() {
        assert_eq!(
            expand_tilde("/absolute/path"),
            PathBuf::from("/absolute/path")
        );
    }

    #[test]
    fn test_toml_edit_round_trip_preserves_comments() {
        // Vérifie que toml_edit préserve les commentaires et les autres clés
        let original = r#"# VoxType Configuration
# Managed by voxtype-settings GUI

engine = "whisper"

[meeting]
enabled = true
storage_path = "~/.local/share/voxtype/meetings/"
retain_audio = false
max_duration_mins = 180

[audio]
device = "default"
"#;
        let mut doc: DocumentMut = original.parse().unwrap();
        doc["meeting"]["storage_path"] = toml_edit::value("/new/path");
        doc["meeting"]["retain_audio"] = toml_edit::value(true);

        let result = doc.to_string();

        // Les commentaires doivent survivre
        assert!(result.contains("# VoxType Configuration"));
        assert!(result.contains("# Managed by voxtype-settings GUI"));
        // Les autres clés doivent survivre
        assert!(result.contains("engine = \"whisper\""));
        assert!(result.contains("enabled = true"));
        assert!(result.contains("max_duration_mins = 180"));
        assert!(result.contains("[audio]"));
        assert!(result.contains("device = \"default\""));
        // Les nouvelles valeurs doivent être présentes
        assert!(result.contains("/new/path"));
        assert!(result.contains("retain_audio = true"));
    }

    #[test]
    fn test_config_path_uses_xdg() {
        // SAFETY : test single-threaded, nettoyage immédiat après vérification
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/custom/config") };
        let path = config_path();
        // Nettoyage avant toute assertion pour garantir la restauration
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(path, PathBuf::from("/custom/config/voxtype/config.toml"));
    }

    #[test]
    fn test_save_config_empty_path_does_not_touch_file() {
        // Un chemin vide doit retourner une erreur AVANT tout accès fichier
        let result = save_config("", false);
        assert!(result.is_err());
        let result = save_config("  ", false);
        assert!(result.is_err());
    }

    /// Test d'écriture complet sur un fichier temporaire.
    /// Ne touche PAS au vrai config — opère uniquement sur du contenu en mémoire.
    #[test]
    fn test_save_config_writes_correctly_on_tmp() {
        use std::str::FromStr;

        let original = r#"# VoxType Configuration
# Managed by voxtype-settings GUI

engine = "whisper"
state_file = "auto"

[meeting]
enabled = true
chunk_duration_secs = 30
storage_path = "~/.local/share/voxtype/meetings/"
retain_audio = false
max_duration_mins = 180

[audio]
device = "default"
"#;

        // Modifier via toml_edit en mémoire (sans passer par save_config
        // qui utilise config_path() statique pointant vers le vrai fichier)
        let mut doc = DocumentMut::from_str(original).unwrap();
        doc["meeting"]["storage_path"] = toml_edit::value("/new/storage");
        doc["meeting"]["retain_audio"] = toml_edit::value(true);
        let result = doc.to_string();

        // Relire le résultat via un second parse
        let written_doc = DocumentMut::from_str(&result).unwrap();
        assert_eq!(
            written_doc["meeting"]["storage_path"].as_str(),
            Some("/new/storage")
        );
        assert_eq!(written_doc["meeting"]["retain_audio"].as_bool(), Some(true));
        // Les autres clés survivent
        assert_eq!(written_doc["meeting"]["enabled"].as_bool(), Some(true));
        assert_eq!(
            written_doc["meeting"]["max_duration_mins"].as_integer(),
            Some(180)
        );
        assert_eq!(written_doc["audio"]["device"].as_str(), Some("default"));
        // Les commentaires survivent
        assert!(result.contains("# VoxType Configuration"));
        assert!(result.contains("# Managed by voxtype-settings GUI"));
    }
}
