use super::input::{normalize_terminal_segment, wrap_input_line};
use crate::selection_state::MAX_SELECTION_VISIBLE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PanelHeights {
    pub(super) activity_height: u16,
    pub(super) pending_messages_height: u16,
    pub(super) provider_status_height: u16,
    pub(super) completion_height: u16,
    pub(super) selection_header_height: u16,
    pub(super) selection_items_height: u16,
    pub(super) halfblock_height: u16,
    pub(super) input_height: u16,
    pub(super) info_height: u16,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PanelInputs {
    pub(super) terminal_height: usize,
    pub(super) input_line_count: usize,
    pub(super) show_info: bool,
    pub(super) selection_mode: bool,
    pub(super) selection_items_len: usize,
    pub(super) completions_len: usize,
    pub(super) resume_hint_visible: bool,
    pub(super) ask_user_selection_no_freeform: bool,
    /// Number of visible lines for the ask_user question in the selection
    /// header (0 when not in ask_user selection mode).  Includes the hints
    /// bar, so the total header rows are `max(1, ask_user_header_lines)`.
    pub(super) ask_user_header_lines: usize,
    pub(super) has_activity: bool,
    pub(super) has_provider_status: bool,
    pub(super) queued_steering_len: usize,
}

pub(super) fn input_visual_line_count(lines: &[String], width: usize) -> usize {
    if width == 0 {
        return lines.len().max(1);
    }

    let mut count = 0usize;
    for line in lines {
        let normalized = normalize_terminal_segment(line, 0);
        count += wrap_input_line(&normalized, width).len();
    }

    count.max(1)
}

pub(super) fn compute_panel_heights(input: PanelInputs) -> PanelHeights {
    let capped_input = input
        .input_line_count
        .max(1)
        .min((input.terminal_height * 40 / 100).max(1)) as u16;

    let info_height: u16 = if input.show_info { 1 } else { 0 };

    let activity_height: u16 = if input.has_activity { 1 } else { 0 };

    let pending_messages_height: u16 = if input.queued_steering_len > 0 {
        input.queued_steering_len.min(3) as u16
    } else {
        0
    };

    let provider_status_height: u16 = if input.has_provider_status { 1 } else { 0 };

    let completion_height = if input.selection_mode {
        0
    } else if input.completions_len > 0 {
        input.completions_len as u16
    } else if input.resume_hint_visible {
        1
    } else {
        0
    };

    let selection_header_height: u16 = if input.selection_mode {
        input.ask_user_header_lines.max(1) as u16
    } else {
        0
    };
    let selection_items_height: u16 = if input.selection_mode {
        input.selection_items_len.clamp(1, MAX_SELECTION_VISIBLE) as u16
    } else {
        0
    };

    let hide_input = input.ask_user_selection_no_freeform;
    let input_height = if hide_input { 0 } else { capped_input };
    let halfblock_height: u16 = if hide_input { 0 } else { 1 };

    PanelHeights {
        activity_height,
        pending_messages_height,
        provider_status_height,
        completion_height,
        selection_header_height,
        selection_items_height,
        halfblock_height,
        input_height,
        info_height,
    }
}

#[cfg(test)]
mod tests {
    use super::{PanelInputs, compute_panel_heights, input_visual_line_count};
    use crate::selection_state::MAX_SELECTION_VISIBLE;

    fn inputs(mut others: impl FnMut(&mut PanelInputs)) -> PanelInputs {
        let mut p = PanelInputs {
            terminal_height: 30,
            input_line_count: 1,
            show_info: false,
            selection_mode: false,
            selection_items_len: 0,
            completions_len: 0,
            resume_hint_visible: false,
            ask_user_selection_no_freeform: false,
            ask_user_header_lines: 0,
            has_activity: false,
            has_provider_status: false,
            queued_steering_len: 0,
        };
        others(&mut p);
        p
    }

    #[test]
    fn input_visual_line_count_wraps_long_lines() {
        let lines = vec!["short".to_string(), "12345 67890".to_string()];
        let count = input_visual_line_count(&lines, 6);
        assert_eq!(count, 3);
    }

    #[test]
    fn layout_uses_visual_input_line_count_for_wrapped_input() {
        let wrapped_lines = input_visual_line_count(&["a very long single line".to_string()], 8);
        assert!(wrapped_lines > 1);

        let heights = compute_panel_heights(inputs(|p| p.input_line_count = wrapped_lines));
        assert_eq!(heights.input_height as usize, wrapped_lines);
    }

    #[test]
    fn layout_hides_completion_when_selection_active() {
        let selection = compute_panel_heights(inputs(|p| {
            p.selection_mode = true;
            p.selection_items_len = 4;
            p.completions_len = 5;
        }));
        assert_eq!(selection.completion_height, 0);
        assert_eq!(selection.selection_items_height, 4);
    }

    #[test]
    fn layout_shows_resume_hint_row_when_applicable() {
        let heights = compute_panel_heights(inputs(|p| p.resume_hint_visible = true));
        assert_eq!(heights.completion_height, 1);
    }

    #[test]
    fn layout_selection_item_rows_are_clamped_to_max_visible() {
        let heights = compute_panel_heights(inputs(|p| {
            p.selection_mode = true;
            p.selection_items_len = MAX_SELECTION_VISIBLE + 10;
        }));

        assert_eq!(heights.selection_header_height, 1);
        assert_eq!(
            heights.selection_items_height as usize,
            MAX_SELECTION_VISIBLE
        );
    }

    #[test]
    fn layout_control_band_rows_follow_visibility_independently() {
        let heights = compute_panel_heights(inputs(|p| {
            p.has_activity = true;
            p.has_provider_status = true;
            p.queued_steering_len = 2;
        }));

        assert_eq!(heights.activity_height, 1);
        assert_eq!(heights.pending_messages_height, 2);
        assert_eq!(heights.provider_status_height, 1);
    }

    #[test]
    fn layout_input_height_is_capped_at_40_percent_of_terminal() {
        let heights = compute_panel_heights(inputs(|p| {
            p.terminal_height = 20;
            p.input_line_count = 99;
        }));
        assert_eq!(heights.input_height, 8);
        assert_eq!(heights.halfblock_height, 1);
    }

    #[test]
    fn layout_info_bar_height_follows_toggle() {
        let hidden = compute_panel_heights(inputs(|_| {}));
        let shown = compute_panel_heights(inputs(|p| p.show_info = true));
        assert_eq!(hidden.info_height, 0);
        assert_eq!(shown.info_height, 1);
    }

    #[test]
    fn layout_handles_small_terminals_without_underflow() {
        let heights = compute_panel_heights(inputs(|p| {
            p.terminal_height = 1;
            p.input_line_count = 0;
            p.show_info = true;
            p.selection_mode = true;
            p.resume_hint_visible = true;
        }));

        assert!(heights.input_height <= 1);
        assert_eq!(heights.selection_header_height, 1);
        assert_eq!(heights.selection_items_height, 1);
    }
}
