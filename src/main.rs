mod settings;

use anyhow::{Context, Result};
use ksni::TrayMethods;
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

// ──────────────────────────────────────────────
// Icônes embarquées (PNG → ARGB32 au premier accès)
// ──────────────────────────────────────────────

static ICON_STOPPED_PNG: &[u8] = include_bytes!("../assets/icon_stopped.png");
static ICON_IDLE_PNG: &[u8] = include_bytes!("../assets/icon_idle.png");
static ICON_ACTIVE_PNG: &[u8] = include_bytes!("../assets/icon_active.png");

fn png_to_ksni_icon(png_bytes: &[u8]) -> Result<ksni::Icon> {
    use std::io::Cursor;
    let img = image::load(Cursor::new(png_bytes), image::ImageFormat::Png)
        .context("Failed to decode embedded PNG icon")?;
    let (width, height) = (img.width(), img.height());
    let mut data = img.into_rgba8().into_vec();
    // RGBA → ARGB (rotation d'un octet à droite par pixel)
    for pixel in data.chunks_exact_mut(4) {
        pixel.rotate_right(1);
    }
    Ok(ksni::Icon {
        width: width as i32,
        height: height as i32,
        data,
    })
}

// Cache des icônes converties (fait une seule fois au démarrage)
struct Icons {
    stopped: ksni::Icon,
    idle: ksni::Icon,
    active: ksni::Icon,
}

impl Icons {
    fn load() -> Result<Self> {
        Ok(Self {
            stopped: png_to_ksni_icon(ICON_STOPPED_PNG)?,
            idle: png_to_ksni_icon(ICON_IDLE_PNG)?,
            active: png_to_ksni_icon(ICON_ACTIVE_PNG)?,
        })
    }
}

// ──────────────────────────────────────────────
// État Voxtype affiché dans le systray
// ──────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum VoxtypeState {
    /// Service systemd inactif ou `voxtype status` injoignable
    Stopped,
    /// Daemon actif, aucune activité (ni réunion ni dictée)
    Idle,
    /// Dictée en cours (recording/transcribing depuis voxtype status)
    ActiveDictation { activity: String },
    /// Réunion en cours (depuis voxtype meeting status)
    ActiveMeeting { meeting_status: String },
}

impl VoxtypeState {
    fn tooltip_title(&self) -> String {
        match self {
            VoxtypeState::Stopped => "Daemon arrêté".to_string(),
            VoxtypeState::Idle => "Au repos (idle)".to_string(),
            VoxtypeState::ActiveDictation { activity } => format!("En cours : {activity}"),
            VoxtypeState::ActiveMeeting { meeting_status } => {
                format!("Réunion en cours ({meeting_status})")
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn is_active(&self) -> bool {
        matches!(
            self,
            VoxtypeState::ActiveDictation { .. } | VoxtypeState::ActiveMeeting { .. }
        )
    }
}

// ──────────────────────────────────────────────
// État interne du canal dictée (voxtype status --follow)
// ──────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum DictationState {
    /// Daemon injoignable / process terminé
    DaemonDown,
    /// Idle
    Idle,
    /// recording ou transcribing
    Active { activity: String },
}

// ──────────────────────────────────────────────
// État interne du canal réunion (voxtype meeting status — polled)
// ──────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum MeetingState {
    /// Aucune réunion en cours
    None,
    /// Réunion active (recording, paused, ou autre valeur non vide)
    Active { status: String },
    /// La commande a échoué ou n'a pas pu être parsée — état inconnu,
    /// on ne l'utilise pas pour décider du rouge (c'est le canal dictée qui gère ça)
    Unknown,
}

// ──────────────────────────────────────────────
// Fusion des deux sources → VoxtypeState affiché
// ──────────────────────────────────────────────

fn combine_states(dictation: &DictationState, meeting: &MeetingState) -> VoxtypeState {
    // Le rouge est exclusivement piloté par le canal dictée (qui vérifie systemd).
    // Un échec du canal réunion seul ne doit jamais mettre au rouge.
    match dictation {
        DictationState::DaemonDown => VoxtypeState::Stopped,
        DictationState::Active { activity } => VoxtypeState::ActiveDictation {
            activity: activity.clone(),
        },
        DictationState::Idle => {
            // Canal dictée idle : on regarde le canal réunion
            match meeting {
                MeetingState::Active { status } => VoxtypeState::ActiveMeeting {
                    meeting_status: status.clone(),
                },
                MeetingState::None | MeetingState::Unknown => VoxtypeState::Idle,
            }
        }
    }
}

// ──────────────────────────────────────────────
// Format JSON émis par `voxtype status --format json`
// ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct VoxtypeStatusJson {
    /// "idle", "recording", "transcribing", "error", "stopped", …
    #[serde(rename = "alt")]
    alt: String,
    /// Tooltip textuel fourni par voxtype (informatif, non utilisé directement)
    #[serde(rename = "tooltip")]
    #[allow(dead_code)]
    tooltip: Option<String>,
}

impl VoxtypeStatusJson {
    fn to_dictation_state(&self) -> DictationState {
        match self.alt.as_str() {
            "idle" => DictationState::Idle,
            "recording" => DictationState::Active {
                activity: "Enregistrement".to_string(),
            },
            "transcribing" => DictationState::Active {
                activity: "Transcription".to_string(),
            },
            "error" | "stopped" => DictationState::DaemonDown,
            other => {
                warn!("État voxtype inconnu : {other:?}, traité comme idle");
                DictationState::Idle
            }
        }
    }
}

// ──────────────────────────────────────────────
// Parser la sortie texte de `voxtype meeting status`
// ──────────────────────────────────────────────

fn parse_meeting_status(output: &str) -> MeetingState {
    // Cas "aucune réunion" : la première ligne contient "No meeting"
    if output.to_lowercase().contains("no meeting") {
        return MeetingState::None;
    }

    // Cherche une ligne "Meeting Status: <valeur>" (insensible à la casse)
    for line in output.lines() {
        let line_lower = line.to_lowercase();
        if let Some(rest) = line_lower.strip_prefix("meeting status:") {
            let status_value = rest.trim().to_string();
            if status_value.is_empty() {
                // Ligne malformée : on ignore
                continue;
            }
            return MeetingState::Active {
                status: status_value,
            };
        }
    }

    // Aucun pattern reconnu
    debug!("voxtype meeting status : sortie non reconnue : {output:?}");
    MeetingState::Unknown
}

// ──────────────────────────────────────────────
// Struct tray ksni
// ──────────────────────────────────────────────

struct VoxtypeTray {
    state: VoxtypeState,
    icons: Arc<Icons>,
}

impl ksni::Tray for VoxtypeTray {
    fn id(&self) -> String {
        "voxtype-systray".to_string()
    }

    fn title(&self) -> String {
        "Voxtype".to_string()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let icon = match &self.state {
            VoxtypeState::Stopped => &self.icons.stopped,
            VoxtypeState::Idle => &self.icons.idle,
            VoxtypeState::ActiveDictation { .. } | VoxtypeState::ActiveMeeting { .. } => {
                &self.icons.active
            }
        };
        vec![ksni::Icon {
            width: icon.width,
            height: icon.height,
            data: icon.data.clone(),
        }]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.state.tooltip_title(),
            description: String::new(),
            icon_name: String::new(),
            icon_pixmap: vec![],
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        // US-02 : menu minimal — Réglages + Quitter (menu de contrôle complet = US-03)
        vec![
            ksni::MenuItem::Standard(ksni::menu::StandardItem {
                label: "Réglages…".to_string(),
                activate: Box::new(|_tray: &mut VoxtypeTray| {
                    // Lance un nouveau process `voxtype-systray settings` sur le thread principal
                    // (egui ne peut pas tourner dans ce callback ksni)
                    match std::env::current_exe() {
                        Ok(exe) => {
                            if let Err(e) = std::process::Command::new(&exe).arg("settings").spawn()
                            {
                                error!("Impossible de lancer la fenêtre de réglages : {e}");
                            }
                        }
                        Err(e) => {
                            error!("Impossible d'obtenir le chemin du binaire courant : {e}");
                        }
                    }
                }),
                ..Default::default()
            }),
            ksni::MenuItem::Separator,
            ksni::MenuItem::Standard(ksni::menu::StandardItem {
                label: "Quitter".to_string(),
                activate: Box::new(|_tray: &mut VoxtypeTray| {
                    std::process::exit(0);
                }),
                ..Default::default()
            }),
        ]
    }
}

// ──────────────────────────────────────────────
// Détection de l'état systemd (fallback / vérification)
// ──────────────────────────────────────────────

async fn systemd_voxtype_active() -> bool {
    match Command::new("systemctl")
        .args(["--user", "is-active", "voxtype"])
        .output()
        .await
    {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            s.trim() == "active"
        }
        Err(e) => {
            debug!("systemctl non disponible : {e}");
            false
        }
    }
}

// ──────────────────────────────────────────────
// Mise à jour du tray depuis les deux sources partagées
// ──────────────────────────────────────────────

async fn push_combined_state(
    handle: &ksni::Handle<VoxtypeTray>,
    dictation: &Arc<Mutex<DictationState>>,
    meeting: &Arc<Mutex<MeetingState>>,
    last_displayed: &Arc<Mutex<VoxtypeState>>,
) {
    let new_state = {
        let d = match dictation.lock() {
            Ok(g) => g.clone(),
            Err(e) => {
                error!("Mutex dictation poisonné : {e}");
                DictationState::DaemonDown
            }
        };
        let m = match meeting.lock() {
            Ok(g) => g.clone(),
            Err(e) => {
                error!("Mutex meeting poisonné : {e}");
                MeetingState::Unknown
            }
        };
        combine_states(&d, &m)
    };

    let changed = match last_displayed.lock() {
        Ok(mut guard) => {
            if *guard != new_state {
                *guard = new_state.clone();
                true
            } else {
                false
            }
        }
        Err(e) => {
            error!("Mutex last_displayed poisonné : {e}");
            true
        }
    };

    if changed {
        info!("Transition d'état → {:?}", new_state);
        let state_clone = new_state.clone();
        handle
            .update(move |tray: &mut VoxtypeTray| {
                tray.state = state_clone;
            })
            .await;
    }
}

// ──────────────────────────────────────────────
// Boucle de suivi dictée via `voxtype status --format json --follow`
// ──────────────────────────────────────────────

async fn dictation_watcher(
    handle: ksni::Handle<VoxtypeTray>,
    dictation_state: Arc<Mutex<DictationState>>,
    meeting_state: Arc<Mutex<MeetingState>>,
    last_displayed: Arc<Mutex<VoxtypeState>>,
) {
    loop {
        // Avant de lancer le process, vérifie si systemd dit que le service tourne
        let systemd_active = systemd_voxtype_active().await;
        if !systemd_active {
            info!("Service voxtype inactif (systemd), état dictée → DaemonDown");
            if let Ok(mut g) = dictation_state.lock() {
                *g = DictationState::DaemonDown;
            }
            push_combined_state(&handle, &dictation_state, &meeting_state, &last_displayed).await;
            sleep(Duration::from_secs(2)).await;
            continue;
        }

        // Lance `voxtype status --format json --follow`
        info!("Démarrage du suivi voxtype status --follow");
        match Command::new("voxtype")
            .args(["status", "--format", "json", "--follow"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Err(e) => {
                error!("Impossible de spawner voxtype : {e}");
                if let Ok(mut g) = dictation_state.lock() {
                    *g = DictationState::DaemonDown;
                }
                push_combined_state(&handle, &dictation_state, &meeting_state, &last_displayed)
                    .await;
                sleep(Duration::from_secs(2)).await;
            }
            Ok(mut child) => {
                let stdout = match child.stdout.take() {
                    Some(s) => s,
                    None => {
                        error!("Pas de stdout depuis voxtype status --follow");
                        if let Ok(mut g) = dictation_state.lock() {
                            *g = DictationState::DaemonDown;
                        }
                        push_combined_state(
                            &handle,
                            &dictation_state,
                            &meeting_state,
                            &last_displayed,
                        )
                        .await;
                        sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };

                let mut reader = BufReader::new(stdout).lines();

                loop {
                    match reader.next_line().await {
                        Ok(Some(line)) if !line.trim().is_empty() => {
                            debug!("voxtype status line: {line}");
                            match serde_json::from_str::<VoxtypeStatusJson>(&line) {
                                Ok(parsed) => {
                                    let new_dictation = parsed.to_dictation_state();
                                    if let Ok(mut g) = dictation_state.lock() {
                                        *g = new_dictation;
                                    }
                                    push_combined_state(
                                        &handle,
                                        &dictation_state,
                                        &meeting_state,
                                        &last_displayed,
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    warn!("JSON malformé de voxtype : {e} — ligne: {line:?}");
                                }
                            }
                        }
                        Ok(Some(_)) => {
                            // ligne vide, on continue
                        }
                        Ok(None) => {
                            info!("voxtype status --follow s'est terminé (EOF)");
                            break;
                        }
                        Err(e) => {
                            warn!("Erreur lecture stdout voxtype : {e}");
                            break;
                        }
                    }
                }

                // Nettoyage du process enfant (best effort)
                let _ = child.kill().await;
                let _ = child.wait().await;

                // Après EOF ou erreur, re-vérifie systemd
                let still_active = systemd_voxtype_active().await;
                if !still_active {
                    if let Ok(mut g) = dictation_state.lock() {
                        *g = DictationState::DaemonDown;
                    }
                    push_combined_state(&handle, &dictation_state, &meeting_state, &last_displayed)
                        .await;
                }

                sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

// ──────────────────────────────────────────────
// Poller du canal réunion via `voxtype meeting status` (toutes les 2 s)
// ──────────────────────────────────────────────

async fn meeting_poller(
    handle: ksni::Handle<VoxtypeTray>,
    dictation_state: Arc<Mutex<DictationState>>,
    meeting_state: Arc<Mutex<MeetingState>>,
    last_displayed: Arc<Mutex<VoxtypeState>>,
) {
    loop {
        match Command::new("voxtype")
            .args(["meeting", "status"])
            .output()
            .await
        {
            Err(e) => {
                // `voxtype` absent ou non exécutable — on logge une seule fois, on n'impacte pas le rouge
                debug!("voxtype meeting status impossible à lancer : {e}");
                if let Ok(mut g) = meeting_state.lock() {
                    *g = MeetingState::Unknown;
                }
            }
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let new_meeting = parse_meeting_status(&stdout);
                debug!("voxtype meeting status → {:?}", new_meeting);

                let changed = match meeting_state.lock() {
                    Ok(mut g) => {
                        if *g != new_meeting {
                            *g = new_meeting;
                            true
                        } else {
                            false
                        }
                    }
                    Err(e) => {
                        error!("Mutex meeting poisonné dans poller : {e}");
                        true
                    }
                };

                if changed {
                    push_combined_state(&handle, &dictation_state, &meeting_state, &last_displayed)
                        .await;
                }
            }
        }

        sleep(Duration::from_secs(2)).await;
    }
}

// ──────────────────────────────────────────────
// Point d'entrée — dispatch multi-mode
// ──────────────────────────────────────────────
//
// Usage :
//   voxtype-systray           → mode tray (défaut)
//   voxtype-systray settings  → fenêtre de réglages egui (thread principal)

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("");

    match mode {
        "settings" => {
            // Mode fenêtre de réglages : egui tourne sur le thread principal,
            // pas besoin de tokio ici.
            if let Err(e) = settings::run_settings_window() {
                eprintln!("voxtype-systray settings erreur: {e:#}");
                std::process::exit(1);
            }
        }
        "" => {
            // Mode tray : runtime tokio current_thread
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Impossible de créer le runtime tokio");
            if let Err(e) = rt.block_on(run_tray()) {
                eprintln!("voxtype-systray erreur fatale: {e:#}");
                std::process::exit(1);
            }
        }
        other => {
            eprintln!("Sous-commande inconnue : {other:?}");
            eprintln!("Usage : voxtype-systray [settings]");
            std::process::exit(2);
        }
    }
}

async fn run_tray() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("voxtype_systray=info".parse()?),
        )
        .init();

    info!("Chargement des icônes…");
    let icons = Arc::new(Icons::load().context("Impossible de charger les icônes embarquées")?);
    info!(
        "Icônes chargées ({} x {} px).",
        icons.idle.width, icons.idle.height
    );

    let tray = VoxtypeTray {
        state: VoxtypeState::Stopped, // état initial pessimiste → rouge
        icons,
    };

    info!("Démarrage du StatusNotifierItem…");
    let handle = tray
        .spawn()
        .await
        .context("Impossible de démarrer le StatusNotifierItem (D-Bus disponible ?)")?;
    info!("Systray enregistré.");

    // État interne des deux canaux — partagés entre les deux tâches de fond
    let dictation_state = Arc::new(Mutex::new(DictationState::DaemonDown));
    let meeting_state = Arc::new(Mutex::new(MeetingState::None));
    // Dernier état effectivement envoyé au tray (pour éviter les updates D-Bus inutiles)
    let last_displayed = Arc::new(Mutex::new(VoxtypeState::Stopped));

    // Tâche 1 : suivi du canal dictée (push --follow)
    tokio::spawn(dictation_watcher(
        handle.clone(),
        Arc::clone(&dictation_state),
        Arc::clone(&meeting_state),
        Arc::clone(&last_displayed),
    ));

    // Tâche 2 : polling du canal réunion (toutes les 2 s)
    tokio::spawn(meeting_poller(
        handle,
        Arc::clone(&dictation_state),
        Arc::clone(&meeting_state),
        Arc::clone(&last_displayed),
    ));

    // Garde le thread principal en vie indéfiniment
    std::future::pending::<()>().await;

    Ok(())
}

// ──────────────────────────────────────────────
// Tests unitaires du parser `voxtype meeting status`
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_meeting_no_meeting() {
        let output = "No meeting currently in progress.\n\nUse 'voxtype meeting list' to see past meetings.\n";
        assert_eq!(parse_meeting_status(output), MeetingState::None);
    }

    #[test]
    fn test_parse_meeting_recording() {
        let output =
            "Meeting Status: recording\nMeeting ID: 1aeb7e38-b91a-46c1-a2c8-e21b6dc12b51\n";
        assert_eq!(
            parse_meeting_status(output),
            MeetingState::Active {
                status: "recording".to_string()
            }
        );
    }

    #[test]
    fn test_parse_meeting_paused() {
        let output = "Meeting Status: paused\nMeeting ID: 1aeb7e38-b91a-46c1-a2c8-e21b6dc12b51\n";
        assert_eq!(
            parse_meeting_status(output),
            MeetingState::Active {
                status: "paused".to_string()
            }
        );
    }

    #[test]
    fn test_parse_meeting_unknown_status() {
        // Un statut futur non reconnu doit quand même être traité comme actif
        let output = "Meeting Status: initializing\nMeeting ID: abc-123\n";
        assert_eq!(
            parse_meeting_status(output),
            MeetingState::Active {
                status: "initializing".to_string()
            }
        );
    }

    #[test]
    fn test_parse_meeting_case_insensitive() {
        // Le parser est insensible à la casse sur la clé
        let output = "MEETING STATUS: recording\nMeeting ID: abc\n";
        assert_eq!(
            parse_meeting_status(output),
            MeetingState::Active {
                status: "recording".to_string()
            }
        );
    }

    #[test]
    fn test_parse_meeting_empty_output() {
        assert_eq!(parse_meeting_status(""), MeetingState::Unknown);
    }

    #[test]
    fn test_combine_daemon_down_overrides_meeting() {
        // Daemon down > réunion active : doit rester rouge
        let d = DictationState::DaemonDown;
        let m = MeetingState::Active {
            status: "recording".to_string(),
        };
        assert_eq!(combine_states(&d, &m), VoxtypeState::Stopped);
    }

    #[test]
    fn test_combine_idle_plus_active_meeting_is_green() {
        let d = DictationState::Idle;
        let m = MeetingState::Active {
            status: "recording".to_string(),
        };
        assert_eq!(
            combine_states(&d, &m),
            VoxtypeState::ActiveMeeting {
                meeting_status: "recording".to_string()
            }
        );
    }

    #[test]
    fn test_combine_idle_plus_no_meeting_is_idle() {
        let d = DictationState::Idle;
        let m = MeetingState::None;
        assert_eq!(combine_states(&d, &m), VoxtypeState::Idle);
    }

    #[test]
    fn test_combine_active_dictation_takes_precedence() {
        let d = DictationState::Active {
            activity: "Enregistrement".to_string(),
        };
        let m = MeetingState::Active {
            status: "recording".to_string(),
        };
        assert_eq!(
            combine_states(&d, &m),
            VoxtypeState::ActiveDictation {
                activity: "Enregistrement".to_string()
            }
        );
    }

    #[test]
    fn test_combine_idle_plus_unknown_meeting_is_idle() {
        // Un échec du poll réunion ne doit pas masquer idle
        let d = DictationState::Idle;
        let m = MeetingState::Unknown;
        assert_eq!(combine_states(&d, &m), VoxtypeState::Idle);
    }

    #[test]
    fn test_tooltip_meeting() {
        let state = VoxtypeState::ActiveMeeting {
            meeting_status: "recording".to_string(),
        };
        assert_eq!(state.tooltip_title(), "Réunion en cours (recording)");
    }

    #[test]
    fn test_tooltip_dictation() {
        let state = VoxtypeState::ActiveDictation {
            activity: "Transcription".to_string(),
        };
        assert_eq!(state.tooltip_title(), "En cours : Transcription");
    }

    #[test]
    fn test_is_active_flags() {
        assert!(!VoxtypeState::Stopped.is_active());
        assert!(!VoxtypeState::Idle.is_active());
        assert!(
            VoxtypeState::ActiveDictation {
                activity: "x".to_string()
            }
            .is_active()
        );
        assert!(
            VoxtypeState::ActiveMeeting {
                meeting_status: "x".to_string()
            }
            .is_active()
        );
    }
}
