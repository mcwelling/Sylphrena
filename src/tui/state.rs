//! Terminal-side state shared by the REPL and its background tasks.
use std::io::Stdout;
use std::sync::{Arc, Mutex};

use syl_container::ContainerStats;

use crate::tui::animation::AnimHandle;
use crate::tui::spinner::SpinnerHandle;

pub struct TuiState {
    /// Shared stdout. Animation, spinner and `print_to_scroll` all lock it.
    pub stdout: Arc<Mutex<Stdout>>,
    /// Live tool spinner, if any.
    pub spinner: Mutex<Option<SpinnerHandle>>,
    /// Live thinking animation, if any.
    pub anim: Mutex<Option<AnimHandle>>,
    /// Latest container CPU and memory snapshot for the top bar.
    pub container_stats: Mutex<Option<ContainerStats>>,
}

impl TuiState {
    pub fn new() -> Arc<Self> {
        Arc::new(TuiState {
            stdout: Arc::new(Mutex::new(std::io::stdout())),
            spinner: Mutex::new(None),
            anim: Mutex::new(None),
            container_stats: Mutex::new(None),
        })
    }

    pub fn stop_anim(&self) {
        if let Some(anim) = self.anim.lock().unwrap().take() {
            anim.stop();
        }
    }

    pub fn stop_spinner(&self) {
        if let Some(sp) = self.spinner.lock().unwrap().take() {
            sp.stop();
        }
    }
}
