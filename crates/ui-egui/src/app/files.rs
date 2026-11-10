use super::{Dialog, EffectcraftApp};

impl EffectcraftApp {
    /// Show the crash-recovery dialog for a previous run that didn't exit cleanly.
    pub fn offer_recovery(&mut self, r: effectcraft_engine::autosave::Recovery) {
        if r.autosave.is_some() {
            self.recovery = Some(r);
            self.dialog = Some(Dialog::Recovery);
        }
    }

    /// Open files the system asked the app to open (an After Effects project given on the command
    /// line, double-clicked or opened with EffectCraft, or picked in the browser's Open Project
    /// dialog), as File ▸ Open does: Save changes? and the losses prompt ask first. They open in
    /// the next frame without a dialog up (after the recovery offer).
    pub fn open_documents(&mut self, paths: Vec<String>) {
        self.open_requests.extend(paths);
    }
}
