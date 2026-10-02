use super::*;
use crate::settings::profile_image;

impl Shell {
    pub(super) fn profile_image_key(&self, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        profile_image::account_key(state.workspace_scope, state.auth_user())
    }

    pub(super) fn choose_profile_image(&mut self, cx: &mut Context<Self>) {
        if self.profile_image_task.is_some() {
            return;
        }
        let Some(account_key) = self.profile_image_key(cx) else {
            return;
        };
        let previous = profile_image::path(&account_key, cx);
        let data_dir = self.data_dir.clone();
        self.close_user_menu(cx);
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose Profile Image".into()),
        });
        self.profile_image_task = Some(cx.spawn(async move |this, cx| {
            let result = match receiver.await {
                Ok(Ok(Some(paths))) => match paths.into_iter().next() {
                    Some(path) => Some(
                        cx.background_executor()
                            .spawn(async move { profile_image::prepare(&path, &data_dir) })
                            .await,
                    ),
                    None => None,
                },
                Ok(Ok(None)) => None,
                _ => Some(Err(
                    "Unable to open the image picker. Try again.".to_string()
                )),
            };
            let _ = this.update(cx, |shell, cx| {
                shell.profile_image_task = None;
                if let Some(result) = result {
                    let result = result.and_then(|prepared| {
                        // Native dialogs can outlive sign-out or a change from another window.
                        if shell.profile_image_key(cx).as_ref() != Some(&account_key) {
                            return Err("The account changed. Choose the image again.".into());
                        }
                        if profile_image::path(&account_key, cx) != previous {
                            return Err(
                                "The profile image changed in another window. Choose it again."
                                    .into(),
                            );
                        }
                        profile_image::install(account_key, prepared, cx)
                    });
                    shell.sidebar_notice = result.err().map(SharedString::from);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn remove_profile_image(&mut self, cx: &mut Context<Self>) {
        if self.profile_image_task.is_some() {
            return;
        }
        let Some(account_key) = self.profile_image_key(cx) else {
            return;
        };
        self.close_user_menu(cx);
        self.sidebar_notice = profile_image::remove(&account_key, cx)
            .err()
            .map(SharedString::from);
        cx.notify();
    }
}
