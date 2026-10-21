<!-- Generated from src/default_keys.toml and the command registry — do not edit; regenerate with `TARAE_BLESS=1 cargo test keymap_reference`. -->
# Default keymap

The keys tarae ships with, from [`src/default_keys.toml`](../../src/default_keys.toml). Command names are the same as Helix's,
so Helix key configs carry over. Rebind anything in `[keys.normal]`, `[keys.select]`, or `[keys.insert]` —
see the [configuration guide](../configuration.md#keys).

In the editor you don't need this page: `space ?` finds any command by what it does, and after a prefix key
(`space`, `g`, `m`, `[`, `]`) a card shows what comes next.

## Normal mode

| Key | Command | Description |
|---|---|---|
| `!` | `shell_insert_output` | Insert a command's output before |
| `"` | `select_register` | Select register for the next command |
| `$` | `shell_keep_pipe` | Keep selections a command accepts |
| `%` | `select_all` | Select whole document |
| `*` | `search_selection` | Search for selection |
| `,` | `keep_primary_selection` | Keep primary selection |
| `.` | `repeat_last_insert` | Repeat last insert |
| `/` | `search` | Search for regex pattern |
| `:` | `command_mode` | Enter command mode |
| `;` | `collapse_selection` | Collapse selection into single cursor |
| `<` | `unindent` | Unindent selection |
| `>` | `indent` | Indent selection |
| `?` | `rsearch` | Reverse search for regex pattern |
| `[` | … | More keys — see `[` below |
| `]` | … | More keys — see `]` below |
| `` ` `` | `switch_to_lowercase` | Switch to lowercase |
| `a` | `append_mode` | Append after selection |
| `A` | `insert_at_line_end` | Insert at end of line |
| `b` | `move_prev_word_start` | Move to start of previous word |
| `c` | `change_selection` | Change selection |
| `C` | `copy_selection_on_next_line` | Copy selection on next line |
| `d` | `delete_selection` | Delete selection |
| `e` | `move_next_word_end` | Move to end of next word |
| `f` | `find_next_char` | Move to next occurrence of char |
| `F` | `find_prev_char` | Move to previous occurrence of char |
| `g` | … | More keys — see `g` below |
| `G` | `goto_line` | Go to last line (or line &lt;n>) |
| `h` | `move_char_left` | Move left |
| `i` | `insert_mode` | Insert before selection |
| `I` | `insert_at_line_start` | Insert at start of line |
| `j` | `move_visual_line_down` | Move down (visual line) |
| `J` | `join_selections` | Join lines inside selection |
| `k` | `move_visual_line_up` | Move up (visual line) |
| `K` | `keep_selections` | Keep selections matching the regex |
| `l` | `move_char_right` | Move right |
| `m` | … | More keys — see `m` below |
| `n` | `search_next` | Select next search match |
| `N` | `search_prev` | Select previous search match |
| `o` | `open_below` | Open new line below selection |
| `O` | `open_above` | Open new line above selection |
| `p` | `paste_after` | Paste after selection |
| `P` | `paste_before` | Paste before selection |
| `q` | `replay_macro` | Replay macro |
| `Q` | `record_macro` | Record macro |
| `r` | `replace` | Replace with new char |
| `s` | `select_regex` | Select regex matches |
| `S` | `split_selection` | Split selections on regex matches |
| `t` | `find_till_char` | Move till next occurrence of char |
| `T` | `till_prev_char` | Move till previous occurrence of char |
| `u` | `undo` | Undo change |
| `U` | `redo` | Redo change |
| `v` | `select_mode` | Enter selection extend mode |
| `w` | `move_next_word_start` | Move to start of next word |
| `x` | `extend_line_below` | Select line (again: extend down) |
| `X` | `extend_line_up` + `extend_to_line_bounds` | Extend up, then extend selection to line bounds |
| `y` | `yank` | Yank selection |
| `z` | … | More keys — see `z` below |
| `Z` | … | More keys — see `Z` below |
| `\|` | `shell_pipe` | Pipe selections through a command |
| `~` | `switch_case` | Switch (toggle) case |
| `A-!` | `shell_append_output` | Insert a command's output after |
| `A-.` | `repeat_last_motion` | Repeat last motion |
| `A-;` | `flip_selections` | Flip selection cursor and anchor |
| `` A-` `` | `switch_to_uppercase` | Switch to uppercase |
| `A-C` | `copy_selection_on_prev_line` | Copy selection on previous line |
| `A-c` | `change_selection_noyank` | Change selection without yanking |
| `A-d` | `delete_selection_noyank` | Delete selection without yanking |
| `A-down` | `shrink_selection` | Shrink selection back |
| `A-i` | `shrink_selection` | Shrink selection back |
| `A-K` | `remove_selections` | Remove selections matching the regex |
| `A-left` | `select_prev_sibling` | Select previous syntax sibling |
| `A-n` | `select_next_sibling` | Select next syntax sibling |
| `A-o` | `expand_selection` | Grow selection to the enclosing syntax node |
| `A-p` | `select_prev_sibling` | Select previous syntax sibling |
| `A-right` | `select_next_sibling` | Select next syntax sibling |
| `A-s` | `split_selection_on_newline` | Split selection on newlines |
| `A-up` | `expand_selection` | Grow selection to the enclosing syntax node |
| `A-x` | `extend_to_line_bounds` | Extend selection to line bounds |
| `A-\|` | `shell_pipe_to` | Send selections to a command |
| `C-\` | `terminal_focus` | Terminal: give it the keys |
| `C-b` | `page_up` | Move page up |
| `C-c` | `toggle_comments` | Comment or uncomment lines |
| `C-d` | `half_page_down` | Move half page down |
| `C-f` | `page_down` | Move page down |
| `C-i` | `jump_forward` | Jump forward again |
| `C-o` | `jump_backward` | Jump back to the previous spot |
| `C-s` | `save_selection` | Save this spot to jump back to |
| `C-u` | `half_page_up` | Move half page up |
| `C-w` | … | More keys — see `C-w` below |
| `down` | `move_visual_line_down` | Move down (visual line) |
| `end` | `goto_line_end` | Go to line end |
| `esc` | `normal_mode` | Enter normal mode |
| `F10` | `dap_next` | Step over |
| `F11` | `dap_step_in` | Step into |
| `F12` | `dap_step_out` | Step out |
| `F5` | `dap_launch` | Start debugging / continue |
| `F9` | `toggle_breakpoint` | Toggle breakpoint |
| `home` | `goto_line_start` | Go to line start |
| `left` | `move_char_left` | Move left |
| `pagedown` | `page_down` | Move page down |
| `pageup` | `page_up` | Move page up |
| `right` | `move_char_right` | Move right |
| `space` | … | More keys — see `space` below |
| `tab` | `jump_forward` | Jump forward again |
| `up` | `move_visual_line_up` | Move up (visual line) |

### `[`

| Key | Command | Description |
|---|---|---|
| `[ a` | `goto_prev_parameter` | Previous argument |
| `[ c` | `goto_prev_comment` | Previous comment |
| `[ d` | `goto_prev_diag` | Previous diagnostic |
| `[ f` | `goto_prev_function` | Previous function |
| `[ F` | `goto_prev_review_file` | Previous file of the review |
| `[ g` | `goto_prev_change` | Previous git hunk |
| `[ G` | `goto_prev_change_part` | Previous change, part by part |
| `[ h` | `goto_prev_high_change` | Previous high-risk change |
| `[ m` | `goto_prev_conflict` | Previous merge conflict |
| `[ n` | `goto_prev_note` | Previous note |
| `[ p` | `goto_prev_paragraph` | Previous paragraph |
| `[ q` | `goto_prev_quickfix` | Previous place in the list |
| `[ r` | `replay_prev` | Replay: Claude's previous stop |
| `[ t` | `goto_prev_class` | Previous type |
| `[ T` | `goto_prev_test` | Previous test |
| `[ u` | `goto_prev_unviewed_file` | Previous file not viewed |
| `[ x` | `goto_prev_test_failure` | Previous failed test |
| `[ space` | `add_newline_above` | Add a blank line above |

### `]`

| Key | Command | Description |
|---|---|---|
| `] a` | `goto_next_parameter` | Next argument |
| `] c` | `goto_next_comment` | Next comment |
| `] d` | `goto_next_diag` | Next diagnostic |
| `] f` | `goto_next_function` | Next function |
| `] F` | `goto_next_review_file` | Next file of the review |
| `] g` | `goto_next_change` | Next git hunk |
| `] G` | `goto_next_change_part` | Next change, part by part |
| `] h` | `goto_next_high_change` | Next high-risk change |
| `] m` | `goto_next_conflict` | Next merge conflict |
| `] n` | `goto_next_note` | Next note |
| `] p` | `goto_next_paragraph` | Next paragraph |
| `] q` | `goto_next_quickfix` | Next place in the list |
| `] r` | `replay_next` | Replay: Claude's next stop |
| `] t` | `goto_next_class` | Next type |
| `] T` | `goto_next_test` | Next test |
| `] u` | `goto_next_unviewed_file` | Next file not viewed |
| `] x` | `goto_next_test_failure` | Next failed test |
| `] space` | `add_newline_below` | Add a blank line below |

### `g`

| Key | Command | Description |
|---|---|---|
| `g a` | `goto_last_accessed_file` | Go to last accessed file |
| `g b` | `goto_window_bottom` | Cursor to the screen bottom |
| `g c` | `goto_window_center` | Cursor to mid-screen |
| `g d` | `goto_definition` | Go to definition (LSP) |
| `g D` | `goto_declaration` | Go to declaration (LSP) |
| `g e` | `goto_last_line` | Go to last line |
| `g g` | `goto_file_start` | Go to file start (or line &lt;n>) |
| `g h` | `goto_line_start` | Go to line start |
| `g i` | `goto_implementation` | Go to implementation (LSP) |
| `g l` | `goto_line_end` | Go to line end |
| `g n` | `goto_next_buffer` | Go to next buffer |
| `g p` | `goto_previous_buffer` | Go to previous buffer |
| `g r` | `goto_reference` | Go to references (LSP) |
| `g s` | `goto_first_nonwhitespace` | Go to first non-blank in line |
| `g t` | `goto_window_top` | Cursor to the screen top |
| `g w` | `goto_word` | Jump to a word by its label |
| `g y` | `goto_type_definition` | Go to type definition (LSP) |

### `m`

| Key | Command | Description |
|---|---|---|
| `m a` | `select_textobject_around` | Select around object |
| `m d` | `surround_delete` | Surround delete |
| `m i` | `select_textobject_inner` | Select inside object |
| `m m` | `match_brackets` | Go to matching bracket |
| `m r` | `surround_replace` | Surround replace |
| `m s` | `surround_add` | Surround add |

### `z`

| Key | Command | Description |
|---|---|---|
| `z b` | `align_view_bottom` | Cursor line to the bottom |
| `z c` | `align_view_center` | Cursor line to mid-screen |
| `z j` | `scroll_down` | Scroll down a line |
| `z k` | `scroll_up` | Scroll up a line |
| `z t` | `align_view_top` | Cursor line to the top |
| `z w` | `toggle_soft_wrap` | Wrap long lines here (on/off) |
| `z z` | `align_view_center` | Cursor line to mid-screen |
| `z C-b` | `page_up` | Move page up |
| `z C-d` | `half_page_down` | Move half page down |
| `z C-f` | `page_down` | Move page down |
| `z C-u` | `half_page_up` | Move half page up |

### `Z`

| Key | Command | Description |
|---|---|---|
| `Z b` | `align_view_bottom` | Cursor line to the bottom |
| `Z c` | `align_view_center` | Cursor line to mid-screen |
| `Z j` | `scroll_down` | Scroll down a line |
| `Z k` | `scroll_up` | Scroll up a line |
| `Z t` | `align_view_top` | Cursor line to the top |
| `Z w` | `toggle_soft_wrap` | Wrap long lines here (on/off) |
| `Z z` | `align_view_center` | Cursor line to mid-screen |
| `Z C-b` | `page_up` | Move page up |
| `Z C-d` | `half_page_down` | Move half page down |
| `Z C-f` | `page_down` | Move page down |
| `Z C-u` | `half_page_up` | Move half page up |

### `C-w`

| Key | Command | Description |
|---|---|---|
| `C-w +` | `bottom_taller` | Taller: the bottom panel |
| `C-w <` | `panel_narrower` | Narrower: chat or side panel |
| `C-w =` | `sizes_auto` | Sizes back to automatic |
| `C-w >` | `panel_wider` | Wider: chat or side panel |
| `C-w e` | `focus_side_panel` | To the side panel and back |
| `C-w h` | `jump_view_left` | Window to the left |
| `C-w j` | `jump_view_down` | Window below |
| `C-w k` | `jump_view_up` | Window above |
| `C-w l` | `jump_view_right` | Window to the right |
| `C-w o` | `wonly` | Close all other windows |
| `C-w q` | `wclose` | Close this window or panel |
| `C-w s` | `hsplit` | Split the window top / bottom |
| `C-w t` | `slot_next` | Next tab of the bottom panel |
| `C-w v` | `vsplit` | Split the window side by side |
| `C-w w` | `rotate_view` | Next window |
| `C-w z` | `zoom_toggle` | Full screen: on / off |
| `C-w C-w` | `rotate_view` | Next window |
| `C-w minus` | `bottom_shorter` | Shorter: the bottom panel |

### `space`

| Key | Command | Description |
|---|---|---|
| `space '` | `last_picker` | Reopen the last picker |
| `space /` | `global_search` | Search in project |
| `space >` | `quote` | Quote or view chat |
| `space ?` | `command_palette` | Find a command |
| `space @` | `chat_point` | Point Claude at something |
| `space a` | `code_action` | Code actions |
| `space b` | `buffer_picker` | Switch to an open buffer |
| `space c` | `toggle_comments` | Comment or uncomment lines |
| `space C` | `toggle_block_comments` | Block-comment the selections |
| `space d` | `diagnostics_picker` | Diagnostics |
| `space D` | `workspace_diagnostics_picker` | Diagnostics in every file |
| `space e` | `file_explorer` | Files panel: show / hide |
| `space f` | `file_picker` | Open a file |
| `space g` | `git_panel` | Git: stage, commit, push |
| `space G` | … | More keys — see `space G` below |
| `space h` | `select_references_to_symbol_under_cursor` | Select this symbol's uses |
| `space i` | `llm_ask` | Ask Claude to edit selection |
| `space I` | `inbox` | What waits for your answer |
| `space j` | `jumplist_picker` | Jump list |
| `space k` | `hover` | Show docs under cursor |
| `space l` | `chat_open` | Chat with Claude |
| `space L` | `chat_close` | Close chat |
| `space m` | … | More keys — see `space m` below |
| `space n` | `note_here` | Ask Claude here, or reply |
| `space N` | `note_picker` | Claude's notes |
| `space o` | `outline` | Outline panel: show / hide |
| `space p` | `paste_clipboard_after` | Paste clipboard after |
| `space P` | `paste_clipboard_before` | Paste clipboard before |
| `space q` | `quickfix_panel` | List panel: show / hide |
| `space r` | `rename_symbol` | Rename symbol |
| `space s` | `symbol_picker` | Symbols in this file |
| `space S` | `workspace_symbol_picker` | Symbols in the workspace |
| `space t` | `theme_picker` | Choose a theme (live preview) |
| `space T` | `terminal` | Terminal: show / hide |
| `space u` | `undo_turn` | Undo Claude's last turn |
| `space U` | `redo_turn` | Bring Claude's turn back |
| `space v` | … | More keys — see `space v` below |
| `space w` | … | More keys — see `space w` below |
| `space W` | `session_list` | Sessions: switch, end, new |
| `space x` | … | More keys — see `space x` below |
| `space y` | `yank_to_clipboard` | Copy to clipboard |

### `space G`

| Key | Command | Description |
|---|---|---|
| `space G a` | `dap_attach` | Attach to a running program… |
| `space G b` | `toggle_breakpoint` | Toggle breakpoint |
| `space G c` | `dap_launch` | Start debugging / continue |
| `space G i` | `dap_step_in` | Step into |
| `space G l` | `dap_launch` | Start debugging / continue |
| `space G n` | `dap_next` | Step over |
| `space G o` | `dap_step_out` | Step out |
| `space G p` | `dap_pause` | Pause the program |
| `space G t` | `dap_terminate` | Stop debugging |
| `space G w` | `dap_watch` | Watch an expression |
| `space G W` | `dap_unwatch` | Remove a watch |
| `space G C-c` | `dap_edit_condition` | Break only when… (condition) |
| `space G C-l` | `dap_edit_log` | Log here instead of stopping |

### `space m`

| Key | Command | Description |
|---|---|---|
| `space m c` | `task_close` | Stop the task, close its panel |
| `space m m` | `task_again` | Run the task (again) |
| `space m p` | `task_picker` | Pick a task to run |
| `space m s` | `task_sandbox` | What the sandbox blocked |

### `space v`

| Key | Command | Description |
|---|---|---|
| `space v a` | `review_at_end` | Claude's edits: review at end |
| `space v b` | `review_branch` | Review against a branch… |
| `space v c` | `review_claude` | Second Claude reviews changes |
| `space v C` | `review_board` | A board reviews the changes |
| `space v h` | `review_hunk_seen` | Looked at this risky change |
| `space v H` | `review_file_seen` | Looked at all drawn in file |
| `space v m` | `review_viewed` | Mark this file viewed |
| `space v M` | `review_unviewed` | Unmark this file viewed |
| `space v q` | `review_close` | End the review |
| `space v r` | `review_verdict` | The reviewer's verdict |
| `space v s` | `review_send` | Send the review to Claude |
| `space v t` | `review_turn` | Review Claude's last turn |
| `space v u` | `review_undo_change` | Take this change back |
| `space v v` | `review_start` | Review the changes |
| `space v z` | `review_wide` | Read wide: hide rail and chat |

### `space w`

| Key | Command | Description |
|---|---|---|
| `space w +` | `bottom_taller` | Taller: the bottom panel |
| `space w <` | `panel_narrower` | Narrower: chat or side panel |
| `space w =` | `sizes_auto` | Sizes back to automatic |
| `space w >` | `panel_wider` | Wider: chat or side panel |
| `space w e` | `focus_side_panel` | To the side panel and back |
| `space w h` | `jump_view_left` | Window to the left |
| `space w j` | `jump_view_down` | Window below |
| `space w k` | `jump_view_up` | Window above |
| `space w l` | `jump_view_right` | Window to the right |
| `space w o` | `wonly` | Close all other windows |
| `space w q` | `wclose` | Close this window or panel |
| `space w s` | `hsplit` | Split the window top / bottom |
| `space w t` | `slot_next` | Next tab of the bottom panel |
| `space w v` | `vsplit` | Split the window side by side |
| `space w w` | `rotate_view` | Next window |
| `space w z` | `zoom_toggle` | Full screen: on / off |
| `space w C-w` | `rotate_view` | Next window |
| `space w minus` | `bottom_shorter` | Shorter: the bottom panel |

### `space x`

| Key | Command | Description |
|---|---|---|
| `space x c` | `test_close` | Close the test panel |
| `space x d` | `test_debug` | Debug the test at the cursor |
| `space x f` | `test_file` | Run this file's tests |
| `space x l` | `test_last` | Run the last test again |
| `space x o` | `test_output` | Test output in a buffer |
| `space x s` | `test_sandbox` | What the sandbox blocked |
| `space x x` | `test_nearest` | Run the test at the cursor |

## Select mode

`v` enters select mode. Every normal-mode key works here too, except these — movement extends the selection instead:

| Key | Command | Description |
|---|---|---|
| `b` | `extend_prev_word_start` | Extend to start of previous word |
| `e` | `extend_next_word_end` | Extend to end of next word |
| `h` | `extend_char_left` | Extend left |
| `j` | `extend_visual_line_down` | Extend down (visual line) |
| `k` | `extend_visual_line_up` | Extend up (visual line) |
| `l` | `extend_char_right` | Extend right |
| `v` | `normal_mode` | Enter normal mode |
| `w` | `extend_next_word_start` | Extend to start of next word |
| `down` | `extend_visual_line_down` | Extend down (visual line) |
| `left` | `extend_char_left` | Extend left |
| `right` | `extend_char_right` | Extend right |
| `up` | `extend_visual_line_up` | Extend up (visual line) |

## Insert mode

| Key | Command | Description |
|---|---|---|
| `A-backspace` | `delete_word_backward` | Delete previous word |
| `A-d` | `delete_word_forward` | Delete next word |
| `A-del` | `delete_word_forward` | Delete next word |
| `backspace` | `delete_char_backward` | Delete previous char |
| `C-d` | `delete_char_forward` | Delete next char |
| `C-h` | `delete_char_backward` | Delete previous char |
| `C-j` | `insert_newline` | Insert newline char |
| `C-k` | `kill_to_line_end` | Delete to end of line |
| `C-r` | `insert_register` | Insert register contents |
| `C-s` | `commit_undo_checkpoint` | Make what's typed an undo step |
| `C-u` | `kill_to_line_start` | Delete to start of line |
| `C-w` | `delete_word_backward` | Delete previous word |
| `C-x` | `completion` | Invoke completion popup (LSP) |
| `del` | `delete_char_forward` | Delete next char |
| `down` | `move_visual_line_down` | Move down (visual line) |
| `end` | `goto_line_end_newline` | Go to end of line (after the last char) |
| `esc` | `normal_mode` | Enter normal mode |
| `home` | `goto_line_start` | Go to line start |
| `left` | `move_char_left` | Move left |
| `pagedown` | `page_down` | Move page down |
| `pageup` | `page_up` | Move page up |
| `ret` | `insert_newline` | Insert newline char |
| `right` | `move_char_right` | Move right |
| `tab` | `insert_tab` | Insert tab char |
| `up` | `move_visual_line_up` | Move up (visual line) |

## View mode

A cursor on what a surface shows — the chat's conversation (`esc` from its input), the terminal's scrollback and screen (`C-\` from its program, `C-w j` to its panel), the run panels' output — a task's, a preflight's, the tests' failure or output (`C-w j` to the panel, a click, the wheel). Normal mode's moves, selections, search and counts work there (yours too: `[keys.normal]` spills in), the rest is said, not done; `enter` quotes what's selected to the chat; `space` is its own menu, what it has of the editor's. Its own keys go over them, and each surface's over those.

### Its own keys (`[keys.view]`)

| Key | Command | Description |
|---|---|---|
| `a` | `view_input` | Back to typing |
| `G` | `view_latest` | To the latest, caught up |
| `i` | `view_input` | Back to typing |
| `q` | `view_leave` | Leave view mode |
| `y` | `view_copy` | Copy, to paste anywhere |
| `down` | `scroll_down` | Scroll down a line |
| `esc` | `view_leave` | Leave view mode |
| `left` | `page_up` | Move page up |
| `ret` | `view_take` | Quote it to the chat, or type |
| `right` | `page_down` | Move page down |
| `space` | … | More keys — see `space` below |
| `up` | `scroll_up` | Scroll up a line |

### `space`

| Key | Command | Description |
|---|---|---|
| `space >` | `quote` | Quote or view chat |
| `space ?` | `command_palette` | Find a command |
| `space I` | `inbox` | What waits for your answer |
| `space l` | `chat_open` | Chat with Claude |

### The terminal's (`[keys.view.terminal]`)

| Key | Command | Description |
|---|---|---|
| `q` | `wclose` | Close this window or panel |
| `backtab` | `slot_prev` | Previous bottom panel tab |
| `C-\` | `view_code` | To the code, at once |
| `tab` | `slot_next` | Next tab of the bottom panel |

### The run panels' — task, preflight, tests (`[keys.view.run]`)

| Key | Command | Description |
|---|---|---|
| `K` | `run_where` | Where Claude's command ran |
| `o` | `preflight_output` | A preflight step's output |
| `q` | `wclose` | Close this window or panel |
| `backtab` | `slot_prev` | Previous bottom panel tab |
| `C-e` | `scroll_down` | Scroll down a line |
| `C-y` | `scroll_up` | Scroll up a line |
| `tab` | `slot_next` | Next tab of the bottom panel |

### The chat's input (`[keys.chat]`)

| Key | Command | Description |
|---|---|---|
| `esc` | `chat_view` | View the chat |

### Taken from the terminal's program (`[keys.terminal]` — every other key is the program's; `to_program` gives one of these back)

| Key | Command | Description |
|---|---|---|
| `C-\` | `terminal_view` | View the terminal |
| `C-w` | `window_keys` | Window keys (C-w …) |

## Cards, reviews and panels

Keys of what's up on the screen — not rebindable, so not in `default_keys.toml`:

| Where | Key | What it does |
|---|---|---|
| A card | `y` `n` | Yes · no (some take `a` this chat, `s` this session, `A` always here); Esc never answers one |
| A card | `C-q` | Give it the keys from wherever they are (the status line says CARD); Esc goes back, unanswered |
| No card here | `C-q` | To the session whose Claude waits for you — its card comes up there; nothing waiting, to a session whose turn ended (`done`) |
| A card cut to fit | `v` | All of it in a buffer — `y` waits until its end was on screen |
| A run card listing others like it | `Y` | Run it and the cards it lists — the same run each, read whole first |
| The stale drafts card | `y` `r` `n` | Close them · list just them (`C-x` closes one) · keep them |
| Claude's change to several files | `y` `r` `v` `n` | Accept them all · review them one by one · every change in a buffer · decline them all |
| An edit's review | `y` `n` `a` `tab` | Accept this change · reject it · accept all left · next |
| An edit's review | Esc | Back to the chat, the change unanswered — `C-q` answers it |
| An edit's review | `C-d` `C-u` `C-f` `C-b` `C-e` `C-y`, wheel | Scroll it — `space`, `:` and `C-w` work over it too |
| Code review rail | click on `◦` | Mark the file viewed (a second click leaves it marked) |
| Code review rail | click on `↑ N more` / `↓ N more`, wheel | Scroll the files |
| Code review | `space v q` | End it — asked on a card while files aren't viewed |
| Task panel | click on a preflight step | Its output; wheel, `C-u` `C-d` scroll |
| Task panel | `o` | A preflight step's whole output in a buffer — the failed one's, else the one in view (`:preflight-output &lt;step>`) |
| Task panel | `K` | Where Claude's command in it ran, and why — its sandbox, the card it ran from, its warnings |
| The chat | click on a command's row end (`ok · sandbox ▲1`) | Why it ran there — its sandbox, the card it ran from, Claude's reason, its warnings |
| The chat | `C-u` `C-d`, `PageUp` `PageDown`, `↑` `↓`, wheel | Scroll the conversation — half the rows in sight, a page, a row; `C-u` and the arrows from an empty input (with text `C-u` clears to the cursor) |
| A list or output panel (side panel, review rail, `:procs`, test, task, terminal, a plugin's) | `j` `k` `↓` `↑` `C-n` `C-p` · `C-e` `C-y` · `C-d` `C-u` · `C-f` `C-b` `PageDown` `PageUp` · `g` `G` `Home` `End` | A row · the view a row, the selection staying unless it would leave it (the wheel too, three rows, while the panel has the keys) · half the rows in sight · all of them · the ends — the same keys everywhere, as the code's; a plugin's panel never gets them |
| Hover text, docs | `C-d` `C-u` · `C-e` `C-y` | Scroll it half its rows · a row |
| The terminal, its program with the keys | `S-pageup` `S-pagedown` | Scroll back a page · forward (the program's next key brings it down; a full-screen program gets them) |
| A picker | `C-n` `C-p` `↓` `↑` `Tab` · `C-d` `C-u` · `PageDown` `PageUp` · `Home` `End` | A row · half the rows in sight · all of them · the first and last |
| A picker of files or places | `C-v` `C-s` · `A-ret` | Open it in a split, beside · under (Helix's keys) · open it, the list kept up |
| A picker | `C-t` · `C-f` `C-b`, wheel over it | The preview away and back · scroll it |

## Inside zellij

zellij's default normal mode takes `C-g` `C-q` `C-p` `C-n` `C-s` `C-t` `C-h` `C-o` `C-b` before tarae sees them — `editor.zellij-keys` (yours only) names the letters zellij takes, these by default; a config of your own (session on `C-y`) names its own, and `editor.zellij-hints = off` says no twins at all (the unlock-first preset). Inside zellij (`ZELLIJ` set) the hints — which-key, the palette, the panels', the tutor — say the same thing without them, the twins below (for the default keys); the bindings stay. A key with no twin works once `C-g` has locked zellij (`C-g` again unlocks it).

| Where | Key | Inside zellij |
|---|---|---|
| normal mode | `z C-b` (`page_up`) | `pageup` |
| normal mode | `Z C-b` (`page_up`) | `pageup` |
| normal mode | `C-b` (`page_up`) | `pageup` |
| normal mode | `C-o` (`jump_backward`) | none |
| normal mode | `C-s` (`save_selection`) | none |
| insert mode | `C-h` (`delete_char_backward`) | `backspace` |
| insert mode | `C-s` (`commit_undo_checkpoint`) | none |
| A card waiting | `C-q` | The card in front: the way back to the code, where it takes the keys — `esc` from insert mode, a prompt, a picker, a panel, a view mode or a chat with nothing said yet · `esc esc` from the chat's input · `C-\ C-\` from the terminal's program. A card in a session behind, an edit, the edit review: the way back, then `space I` (the inbox, `enter` goes to it). A turn that ended: the way back, then `space W` |
| The chat's input | `C-t` · `C-p` · `C-s` · `C-o` | The way back to the code (`esc esc`, `esc` with nothing said yet), then `:model` · `:chat-resume` · `:chat-screen` · `:chat-point` — in the input `:` is a character |
| A list or output panel | `C-n` `C-p` · `C-b` | `j` `k` · `PageUp` |
| A picker | `C-n` `C-p` · `C-b` · `C-t` | `↓` `↑` · the wheel over the preview · none (the preview away) |
| A picker of files or places | `C-s` · `C-q` | none (`C-v` opens it beside) · none (the places kept as the list) |

## `:` commands

Type `:` in normal mode. `Tab` completes command names and their arguments (paths, themes, settings, languages).

| Command | Aliases | Description |
|---|---|---|
| `:write` | `:w` | Save (or save as a new path) |
| `:write!` | `:w!` | Save, overwriting changes made on disk |
| `:write-quit` | `:wq` `:x` | Save and quit |
| `:write-all` | `:wa` | Save every modified file |
| `:write-quit-all` | `:wqa` `:xa` | Save every modified file and quit |
| `:quit` | `:q` | Close this window, or quit |
| `:quit!` | `:q!` | Quit without saving |
| `:quit-all` | `:qa` | Quit tarae |
| `:quit-all!` | `:qa!` | Quit tarae without saving |
| `:restart` |  | Start tarae again, as you were (after an update) |
| `:restart!` |  | Restart, skipping editor.restart-build |
| `:preflight` |  | Run the project's quick checks |
| `:preflight-full` |  | Run every check, before publishing |
| `:preflight-output` |  | A preflight step's output in a buffer |
| `:open` | `:o` `:e` `:edit` | Open files |
| `:new` | `:n` | New scratch buffer |
| `:reload` |  | Reload the file from disk (u brings your version back) |
| `:buffer-close` | `:bc` `:bclose` | Close this buffer |
| `:buffer-close!` | `:bc!` `:bclose!` | Close this buffer, dropping changes |
| `:buffer-next` | `:bn` `:bnext` | Next buffer |
| `:buffer-previous` | `:bp` `:bprev` | Previous buffer |
| `:vsplit` | `:vs` | Split side by side (optionally open a file there) |
| `:hsplit` | `:hs` `:sp` `:split` | Split top and bottom (optionally open a file there) |
| `:only` |  | Close every other window |
| `:close` |  | Close this window |
| `:theme` |  | Switch theme (no name = picker with live preview) |
| `:set` |  | Change a setting until you quit (no value = show it) |
| `:set!` |  | Change a setting and save it to your config |
| `:toggle` |  | Flip an on/off setting |
| `:config-open` |  | Open your config file |
| `:config-reload` |  | Reload your config file |
| `:config-show` |  | Show every setting with its value |
| `:project-trust` |  | Look again at what this project's .tarae.toml runs, and trust it or not |
| `:project-untrust` |  | Stop running what this project's .tarae.toml sets |
| `:claude-settings` |  | Look again at what this project's Claude settings run in the chat, and trust them or not |
| `:git-trust` |  | Look again at what this repository's own git settings run, and trust them or not |
| `:claude-update` |  | Look for a newer claude and install it (claude update, as you) |
| `:claude-restart` |  | Start every chat's claude again, on the login it finds now |
| `:granted` | `:sandbox-allowed` | What you've let through — take one back |
| `:sandbox-disallow` |  | Take back what Claude may do without asking |
| `:sandbox-blocked` |  | What the sandbox blocked in Claude's last run: open it |
| `:sandbox-env` |  | Your environment's names Claude's commands get, and don't |
| `:chat-screen` |  | Send the screen, as text, with your next chat message |
| `:chat-point` |  | Point at something on screen — it goes with your next chat message |
| `:quote` |  | Quote the selection to the chat (space >) — none: view the chat |
| `:log-open` |  | Open tarae's log (language servers, errors) |
| `:procs` |  | What tarae runs and what it costs |
| `:messages` |  | The notifications since tarae started, newest first |
| `:inbox` |  | What waits for your answer, every session's, oldest first |
| `:usage` |  | What Claude's turns cost here and today, and its cache |
| `:keep-warm` |  | Keep this session's Claude cache warm while it waits (on/off/now) |
| `:features` |  | Which features you use, and which never (features.count) |
| `:features-reset` |  | Empty the feature counts (a card asks first) |
| `:shuttle-end` |  | Stop the program typing into tarae, and its grant |
| `:redraw` |  | Draw the whole screen again (when something else wrote over it) |
| `:lsp-restart` |  | Restart this file's language server |
| `:plugin-permissions` |  | What each plugin may do beyond the buffer |
| `:plugin-revoke` |  | Take back a plugin's permissions: :plugin-revoke &lt;plugin> |
| `:plugin-approve` |  | A plugin's approval card again: :plugin-approve &lt;plugin> |
| `:lsp-stop` |  | Stop this file's language server |
| `:format` | `:fmt` | Format the file (language server) |
| `:set-language` | `:lang` | Set this buffer's language |
| `:grammar-install` |  | Fetch and build syntax grammars (this file's language, a name, or all) |
| `:run-shell-command` | `:sh` | Run a shell command in the background |
| `:pipe` | `:\|` | Pipe the selections through a command (output replaces them) |
| `:pipe-to` |  | Send the selections to a command (output ignored) |
| `:insert-output` |  | Insert a command's output before each selection |
| `:append-output` |  | Insert a command's output after each selection |
| `:ask` |  | Ask Claude to edit the selection |
| `:ask-cancel` |  | Cancel the Claude request |
| `:chat` |  | Open the Claude chat (optionally send a message) |
| `:chat-close` |  | Close the Claude chat |
| `:chat-new` |  | Start a new Claude chat |
| `:chat-resume` |  | Continue an earlier Claude conversation |
| `:model` |  | Choose Claude's model (none: a picker) |
| `:effort` |  | How hard Claude thinks (none: a picker) |
| `:review` |  | Review the changes (uncommitted, or since a branch) |
| `:review-send` |  | Send the review's comments to Claude |
| `:review-claude` |  | A second Claude reviews the changes (uncommitted, or since a branch), in the background |
| `:review-board` |  | A board of Claudes reviews the changes in the background, each from its angle, a skeptic checking each finding |
| `:review-verdict` |  | The reviewer's verdict and findings |
| `:review-hunk-seen` |  | The high-risk change here: looked at |
| `:review-file-seen` |  | The file's high-risk changes all drawn: looked at |
| `:reviewer-stop` |  | Stop the reviewer reading |
| `:review-close` |  | End the review |
| `:review-at-end` |  | Claude's edits go in as it works, reviewed when its turn ends (on/off) |
| `:review-turn` |  | Review what Claude changed in its last turn |
| `:soft-wrap` |  | Wrap this buffer's long lines, or scroll sideways (on/off) |
| `:note-close` |  | Close Claude's note on this line |
| `:notes-clear` |  | Close all notes (or one author's: reviewer, author, you) |
| `:commit` |  | Commit every change from a review all viewed (message file) |
| `:commit-draft` |  | Let Claude draft the commit message |
| `:undo-turn` |  | Undo what Claude changed in its last turn |
| `:redo-turn` |  | Bring back the turn you undid |
| `:attach` |  | Attach the debugger to a running program (name or host:port) |
| `:watch` |  | Watch an expression while debugging |
| `:unwatch` |  | Stop watching (no expression = all) |
| `:tutor` |  | Learn the keys — a 10-minute tutorial |
| `:task` | `:make` | Run a build task (by name; none: pick one) |
| `:git-switch` |  | Switch to a branch (none: pick one) |
| `:git-branch-new` |  | Make a branch and switch to it |
| `:git-branch-rename` |  | Rename a branch (one name: this one) |
| `:git-branch-delete` |  | Delete a branch (asks if it loses commits) |
| `:git-amend` |  | Amend the last commit (as A) |
| `:diff` |  | Show changes against HEAD, a file, or a revision |
| `:diff-off` |  | Stop showing the diff |
| `:session-open` |  | Work in another checkout beside this one |
| `:session-switch` |  | Bring another session to the front |
| `:session-new` |  | New session: a branch in its own checkout |
| `:session-end` |  | End a session (its checkout goes) |
| `:session-bring` |  | Move a session's base up to it (rebase first if it moved) |
| `:session-apply` |  | Bring a patch in as one reviewed change set ([--base &lt;rev>] &lt;patch>) |
| `:session-billing` |  | Who pays for this session's Claude: api (your API key) or subscription — the conversation goes on |
| `:rebase-continue` |  | Stage the resolved files and go on with the rebase |
| `:rebase-abort` |  | Abort the rebase: back to where it was |
