use anyhow::{Context as _, Result, bail};
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, PathPromptOptions, SharedString, WeakEntity, Window,
};
use notifications::status_toast::StatusToast;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
use ui::{
    Button, Color, Icon, IconName, IconSize, IntoElement, Label, LabelSize, ParentElement, Render,
    Styled, StyledExt, div, h_flex, prelude::*, rems,
};
use util::command::new_command;
use workspace::{ModalView, Workspace};

pub struct FossilCloneModal {
    workspace: WeakEntity<Workspace>,
    repo_input: Entity<editor::Editor>,
    focus_handle: FocusHandle,
}

impl FossilCloneModal {
    pub fn show(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let repo_input = cx.new(|cx| {
            let mut editor = editor::Editor::single_line(window, cx);
            editor.set_placeholder_text("Enter Fossil repository URL...", window, cx);
            editor
        });
        let focus_handle = repo_input.focus_handle(cx);

        window.focus(&focus_handle, cx);

        Self {
            workspace,
            repo_input,
            focus_handle,
        }
    }
}

impl Focusable for FossilCloneModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FossilCloneModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .elevation_3(cx)
            .w(rems(34.))
            .flex_1()
            .overflow_hidden()
            .child(
                div()
                    .w_full()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.repo_input.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .p_2()
                    .gap_0p5()
                    .rounded_b_sm()
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        Label::new("Clone a Fossil repository and open a checkout.")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .child(
                        Button::new("learn-more", "Learn More")
                            .label_size(LabelSize::Small)
                            .end_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::XSmall))
                            .on_click(|_, _, cx| {
                                cx.open_url(
                                    "https://fossil-scm.org/home/doc/trunk/www/ckout-workflows.md",
                                );
                            }),
                    ),
            )
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(DismissEvent);
            }))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                let repo_url = this.repo_input.read(cx).text(cx);
                clone_remote(repo_url.into(), this.workspace.clone(), window, cx);
                cx.emit(DismissEvent);
            }))
    }
}

impl EventEmitter<DismissEvent> for FossilCloneModal {}

impl ModalView for FossilCloneModal {}

pub fn clone_remote(
    repo_url: SharedString,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let destination_prompt = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Select Parent Directory for Fossil Checkout".into()),
    });

    window
        .spawn(cx, async move |cx| {
            let mut paths = destination_prompt.await.ok()?.ok()??;
            let destination_dir = paths.pop()?;
            let repo_url = repo_url.to_string();
            let checkout_name = checkout_name_from_url(&repo_url);
            let result = match workspace
                .update(cx, |_workspace, cx| {
                    let destination_dir = destination_dir.clone();
                    let repo_url = repo_url.clone();
                    cx.background_spawn(async move {
                        fossil_clone_to_checkout(&repo_url, &destination_dir).await
                    })
                })
                .ok()?
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    prompt_setup_error("Fossil Clone", error, cx).await;
                    return None;
                }
            };

            open_checkout_after_setup(
                workspace,
                result.checkout_dir,
                format!("Fossil Clone: {checkout_name}"),
                cx,
            )
            .await;
            Some(())
        })
        .detach();
}

pub fn open_existing_repository(
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let repository_prompt = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Select Fossil Repository Database".into()),
    });

    window
        .spawn(cx, async move |cx| {
            let mut paths = repository_prompt.await.ok()?.ok()??;
            let repository_db = paths.pop()?;

            let checkout_prompt = cx
                .update(|_window, cx| {
                    cx.prompt_for_paths(PathPromptOptions {
                        files: false,
                        directories: true,
                        multiple: false,
                        prompt: Some("Select Checkout Directory".into()),
                    })
                })
                .ok()?;
            let mut paths = checkout_prompt.await.ok()?.ok()??;
            let checkout_dir = paths.pop()?;
            let display_name = checkout_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("repository")
                .to_string();

            if let Err(error) = workspace
                .update(cx, |_workspace, cx| {
                    let repository_db = repository_db.clone();
                    let checkout_dir = checkout_dir.clone();
                    cx.background_spawn(async move {
                        fossil_open_repository(&repository_db, &checkout_dir).await
                    })
                })
                .ok()?
                .await
            {
                prompt_setup_error("Fossil Open Repository", error, cx).await;
                return None;
            }

            open_checkout_after_setup(
                workspace,
                checkout_dir,
                format!("Fossil Repository: {display_name}"),
                cx,
            )
            .await;
            Some(())
        })
        .detach();
}

pub fn init_repository(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    let checkout_prompt = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Select Checkout Directory".into()),
    });

    window
        .spawn(cx, async move |cx| {
            let mut paths = checkout_prompt.await.ok()?.ok()??;
            let checkout_dir = paths.pop()?;
            let display_name = checkout_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("repository")
                .to_string();

            if let Err(error) = workspace
                .update(cx, |_workspace, cx| {
                    let checkout_dir = checkout_dir.clone();
                    cx.background_spawn(async move { fossil_init_checkout(&checkout_dir).await })
                })
                .ok()?
                .await
            {
                prompt_setup_error("Fossil Init", error, cx).await;
                return None;
            }

            open_checkout_after_setup(
                workspace,
                checkout_dir,
                format!("Fossil Repository: {display_name}"),
                cx,
            )
            .await;
            Some(())
        })
        .detach();
}

struct FossilSetupResult {
    checkout_dir: PathBuf,
}

async fn fossil_clone_to_checkout(repo_url: &str, parent_dir: &Path) -> Result<FossilSetupResult> {
    if repo_url.trim().is_empty() {
        bail!("Fossil repository URL is required");
    }

    let checkout_name = checkout_name_from_url(repo_url);
    let checkout_dir = parent_dir.join(&checkout_name);
    let repository_db = parent_dir.join(format!("{checkout_name}.fossil"));

    ensure_path_available(&repository_db)?;
    ensure_path_available(&checkout_dir)?;

    run_fossil(
        parent_dir,
        [
            OsString::from("clone"),
            OsString::from(repo_url),
            repository_db.as_os_str().to_owned(),
        ],
    )
    .await?;
    run_fossil(
        parent_dir,
        [
            OsString::from("open"),
            repository_db.as_os_str().to_owned(),
            OsString::from("--workdir"),
            checkout_dir.as_os_str().to_owned(),
        ],
    )
    .await?;

    Ok(FossilSetupResult { checkout_dir })
}

async fn fossil_open_repository(repository_db: &Path, checkout_dir: &Path) -> Result<()> {
    if !repository_db.is_file() {
        bail!(
            "Fossil repository database does not exist: {}",
            repository_db.display()
        );
    }

    run_fossil(
        checkout_dir.parent().unwrap_or(checkout_dir),
        [
            OsString::from("open"),
            repository_db.as_os_str().to_owned(),
            OsString::from("--workdir"),
            checkout_dir.as_os_str().to_owned(),
        ],
    )
    .await
}

async fn fossil_init_checkout(checkout_dir: &Path) -> Result<()> {
    if !checkout_dir.is_dir() {
        bail!(
            "Checkout directory does not exist: {}",
            checkout_dir.display()
        );
    }

    let repository_db = repository_db_for_checkout(checkout_dir)?;
    ensure_path_available(&repository_db)?;

    run_fossil(
        repository_db.parent().unwrap_or(checkout_dir),
        [OsString::from("init"), repository_db.as_os_str().to_owned()],
    )
    .await?;
    run_fossil(
        checkout_dir,
        [
            OsString::from("open"),
            repository_db.as_os_str().to_owned(),
            OsString::from("--workdir"),
            checkout_dir.as_os_str().to_owned(),
            OsString::from("--force"),
        ],
    )
    .await
}

async fn run_fossil(working_dir: &Path, args: impl IntoIterator<Item = OsString>) -> Result<()> {
    let output = new_command("fossil")
        .current_dir(working_dir)
        .args(args)
        .output()
        .await
        .context("running fossil")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!("fossil command failed: {message}");
    }

    Ok(())
}

async fn open_checkout_after_setup(
    workspace: WeakEntity<Workspace>,
    checkout_dir: PathBuf,
    title: String,
    cx: &mut gpui::AsyncWindowContext,
) {
    let has_worktrees = workspace
        .read_with(cx, |workspace, cx| {
            workspace.project().read(cx).worktrees(cx).next().is_some()
        })
        .unwrap_or(false);

    let prompt_answer = if has_worktrees {
        let prompt = cx.update(|window, cx| {
            window.prompt(
                gpui::PromptLevel::Info,
                &title,
                None,
                &["Add checkout to project", "Open checkout in new project"],
                cx,
            )
        });
        match prompt {
            Ok(prompt) => prompt.await.unwrap_or(0),
            Err(_) => 0,
        }
    } else {
        0
    };

    match prompt_answer {
        0 => {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let checkout_dir = checkout_dir.clone();
                    let create_task = workspace.project().update(cx, |project, cx| {
                        project.create_worktree(&checkout_dir, true, cx)
                    });
                    let workspace_weak = cx.weak_entity();
                    cx.spawn_in(window, async move |_window, cx| match create_task.await {
                        Ok(_) => {
                            workspace_weak
                                .update(cx, |workspace, cx| {
                                    show_status_toast(
                                        workspace,
                                        format!(
                                            "Opened Fossil checkout {}",
                                            checkout_dir.display()
                                        ),
                                        false,
                                        cx,
                                    );
                                })
                                .ok();
                        }
                        Err(error) => {
                            workspace_weak
                                .update(cx, |workspace, cx| {
                                    show_status_toast(
                                        workspace,
                                        format!("Failed to open Fossil checkout: {error:#}"),
                                        true,
                                        cx,
                                    );
                                })
                                .ok();
                        }
                    })
                    .detach();
                })
                .ok();
        }
        1 => {
            workspace
                .update(cx, move |workspace, cx| {
                    let app_state = workspace.app_state().clone();
                    let checkout_dir = checkout_dir.clone();

                    workspace::open_new(
                        Default::default(),
                        app_state,
                        cx,
                        move |workspace, window, cx| {
                            cx.activate(true);
                            let checkout_dir = checkout_dir.clone();
                            let create_task = workspace.project().update(cx, |project, cx| {
                                project.create_worktree(&checkout_dir, true, cx)
                            });
                            let workspace_weak = cx.weak_entity();
                            cx.spawn_in(window, async move |_window, cx| {
                                if let Err(error) = create_task.await {
                                    workspace_weak
                                        .update(cx, |workspace, cx| {
                                            show_status_toast(
                                                workspace,
                                                format!(
                                                    "Failed to open Fossil checkout: {error:#}"
                                                ),
                                                true,
                                                cx,
                                            );
                                        })
                                        .ok();
                                }
                            })
                            .detach();
                        },
                    )
                    .detach();
                })
                .ok();
        }
        _ => {}
    }
}

async fn prompt_setup_error(
    title: &'static str,
    error: anyhow::Error,
    cx: &mut gpui::AsyncWindowContext,
) {
    let detail = format!("{error:#}");
    if let Ok(prompt) = cx.update(|window, cx| {
        window.prompt(
            gpui::PromptLevel::Critical,
            title,
            Some(&detail),
            &["Ok"],
            cx,
        )
    }) {
        prompt.await.ok();
    }
}

fn show_status_toast(
    workspace: &mut Workspace,
    message: impl Into<SharedString>,
    is_error: bool,
    cx: &mut Context<Workspace>,
) {
    let toast = StatusToast::new(message, cx, move |this, _| {
        let icon = if is_error {
            Icon::new(IconName::XCircle)
                .size(IconSize::Small)
                .color(Color::Error)
        } else {
            Icon::new(IconName::Check)
                .size(IconSize::Small)
                .color(Color::Success)
        };
        this.icon(icon).dismiss_button(true)
    });
    workspace.toggle_status_toast(toast, cx);
}

fn ensure_path_available(path: &Path) -> Result<()> {
    if path.exists() {
        bail!("Path already exists: {}", path.display());
    }
    Ok(())
}

fn repository_db_for_checkout(checkout_dir: &Path) -> Result<PathBuf> {
    let name = checkout_dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("repository");
    let parent = checkout_dir
        .parent()
        .context("checkout directory has no parent")?;
    Ok(parent.join(format!("{name}.fossil")))
}

fn checkout_name_from_url(repo_url: &str) -> String {
    let trimmed = repo_url.trim().trim_end_matches('/');
    let last_segment = trimmed
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or("repository");
    let name = last_segment
        .strip_suffix(".fossil")
        .or_else(|| last_segment.strip_suffix(".git"))
        .unwrap_or(last_segment);
    sanitize_checkout_name(name)
}

fn sanitize_checkout_name(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "repository".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_checkout_name_from_fossil_urls() {
        assert_eq!(
            checkout_name_from_url("https://fossil-scm.org/home"),
            "home"
        );
        assert_eq!(
            checkout_name_from_url("https://example.com/repo/project.fossil"),
            "project"
        );
        assert_eq!(
            checkout_name_from_url("https://example.com/repo/my project/"),
            "my-project"
        );
        assert_eq!(checkout_name_from_url(""), "repository");
    }

    #[test]
    fn places_new_repository_database_next_to_checkout() {
        assert_eq!(
            repository_db_for_checkout(Path::new("/tmp/project")).unwrap(),
            PathBuf::from("/tmp/project.fossil")
        );
    }
}
