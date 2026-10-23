/*
 * A narrow C shim over the pinned libghostty-vt C API (doc 02
 * §GhosttyBackend, pin in §libghostty-vt pin).
 *
 * libghostty-vt's API is built on sized structs and tagged unions. Their
 * layouts come from the C compiler here, so the Rust side (`ffi.rs`) only
 * declares the plain `wt_ghostty_*` functions below and two flat structs.
 * Compiled by build.rs only with the `backend-ghostty` feature.
 */

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include <ghostty/vt.h>

/* Effect kinds delivered to the Rust callback (mirrored in ffi.rs). */
#define WT_EFFECT_PTY 1   /* bytes the terminal answers with (DA, DSR, …) */
#define WT_EFFECT_BELL 2
#define WT_EFFECT_TITLE 3 /* the new title, UTF-8 */

/* Cell flags (mirrored in ffi.rs). */
#define WT_CELL_BOLD (1u << 0)
#define WT_CELL_ITALIC (1u << 1)
#define WT_CELL_UNDERLINE (1u << 2)
#define WT_CELL_STRIKE (1u << 3)
#define WT_CELL_INVERSE (1u << 4)
#define WT_CELL_SELECTED (1u << 5)
#define WT_CELL_INVISIBLE (1u << 6)
#define WT_CELL_FAINT (1u << 7)

/* Modifier bits accepted by wt_ghostty_encode_key (mirrored in ffi.rs). */
#define WT_MOD_SHIFT (1u << 0)
#define WT_MOD_CTRL (1u << 1)
#define WT_MOD_ALT (1u << 2)
#define WT_MOD_SUPER (1u << 3)

/* One key press for wt_ghostty_encode_key. */
typedef struct {
  uint32_t keysym;    /* GDK keyval = X11 keysym */
  uint32_t unshifted; /* codepoint without Shift, 0 if none */
  uint32_t mods;      /* WT_MOD_* */
  const char *utf8;   /* the text the key produces; not NUL-terminated */
  size_t utf8_len;
} WtKey;

/* Mouse actions and buttons for wt_ghostty_encode_mouse (mirrored in
 * ffi.rs). */
#define WT_MOUSE_PRESS 0
#define WT_MOUSE_RELEASE 1
#define WT_MOUSE_MOTION 2
#define WT_BUTTON_NONE 0
#define WT_BUTTON_LEFT 1
#define WT_BUTTON_WHEEL_UP 4
#define WT_BUTTON_WHEEL_DOWN 5

/* One mouse event for wt_ghostty_encode_mouse. */
typedef struct {
  uint32_t action; /* WT_MOUSE_* */
  uint32_t button; /* WT_BUTTON_* */
  uint32_t mods;   /* WT_MOD_* */
  float x;         /* widget pixels */
  float y;
  uint32_t width; /* widget size in pixels */
  uint32_t height;
  uint32_t cell_w; /* cell size in pixels */
  uint32_t cell_h;
  uint32_t any_pressed; /* a button is held, this event's included */
} WtMouse;

typedef void (*WtEffectFn)(void *userdata, int kind, const uint8_t *data,
                           size_t len);

/* One grid cell, colors already resolved to RGB. */
typedef struct {
  uint32_t codepoint; /* 0 = empty */
  uint8_t fg[3];
  uint8_t bg[3];
  uint8_t flags;
  uint8_t has_bg; /* 0 = default background */
} WtCell;

/* Frame-wide state from the render snapshot. */
typedef struct {
  uint16_t cols;
  uint16_t rows;
  uint16_t cursor_x;
  uint16_t cursor_y;
  uint8_t cursor_visible;
  uint8_t fg[3];
  uint8_t bg[3];
  uint8_t cursor_rgb[3];
  uint8_t cursor_has_rgb;
} WtFrame;

typedef struct {
  GhosttyTerminal terminal;
  GhosttyRenderState render;
  GhosttyRenderStateRowIterator rows;
  GhosttyRenderStateRowCells cells;
  GhosttyKeyEncoder encoder;
  GhosttyKeyEvent key_event;
  GhosttyMouseEncoder mouse_encoder;
  GhosttyMouseEvent mouse_event;
  WtEffectFn effect;
  void *userdata;
} WtGhostty;

static void on_write_pty(GhosttyTerminal t, void *ud, const uint8_t *data,
                         size_t len) {
  (void)t;
  WtGhostty *g = ud;
  if (g->effect) g->effect(g->userdata, WT_EFFECT_PTY, data, len);
}

static void on_bell(GhosttyTerminal t, void *ud) {
  (void)t;
  WtGhostty *g = ud;
  if (g->effect) g->effect(g->userdata, WT_EFFECT_BELL, NULL, 0);
}

static void on_title(GhosttyTerminal t, void *ud) {
  WtGhostty *g = ud;
  GhosttyString title = {0};
  if (ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_TITLE, &title) !=
      GHOSTTY_SUCCESS)
    return;
  if (g->effect)
    g->effect(g->userdata, WT_EFFECT_TITLE, (const uint8_t *)title.ptr,
              title.len);
}

void wt_ghostty_free(WtGhostty *g) {
  if (!g) return;
  ghostty_mouse_event_free(g->mouse_event);
  ghostty_mouse_encoder_free(g->mouse_encoder);
  ghostty_key_event_free(g->key_event);
  ghostty_key_encoder_free(g->encoder);
  ghostty_render_state_row_cells_free(g->cells);
  ghostty_render_state_row_iterator_free(g->rows);
  ghostty_render_state_free(g->render);
  ghostty_terminal_free(g->terminal);
  free(g);
}

WtGhostty *wt_ghostty_new(uint16_t cols, uint16_t rows) {
  WtGhostty *g = calloc(1, sizeof(WtGhostty));
  if (!g) return NULL;
  if (ghostty_terminal_new(NULL, &g->terminal, cols, rows) != GHOSTTY_SUCCESS ||
      ghostty_render_state_new(NULL, &g->render) != GHOSTTY_SUCCESS ||
      ghostty_render_state_row_iterator_new(NULL, &g->rows) !=
          GHOSTTY_SUCCESS ||
      ghostty_render_state_row_cells_new(NULL, &g->cells) != GHOSTTY_SUCCESS ||
      ghostty_key_encoder_new(NULL, &g->encoder) != GHOSTTY_SUCCESS ||
      ghostty_key_event_new(NULL, &g->key_event) != GHOSTTY_SUCCESS ||
      ghostty_mouse_encoder_new(NULL, &g->mouse_encoder) != GHOSTTY_SUCCESS ||
      ghostty_mouse_event_new(NULL, &g->mouse_event) != GHOSTTY_SUCCESS) {
    wt_ghostty_free(g);
    return NULL;
  }
  ghostty_terminal_set(g->terminal, GHOSTTY_TERMINAL_OPT_USERDATA, g);
  ghostty_terminal_set(g->terminal, GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                       (const void *)on_write_pty);
  ghostty_terminal_set(g->terminal, GHOSTTY_TERMINAL_OPT_BELL,
                       (const void *)on_bell);
  ghostty_terminal_set(g->terminal, GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
                       (const void *)on_title);
  return g;
}

/* Effects fire synchronously inside wt_ghostty_write and wt_ghostty_paste. */
void wt_ghostty_set_effect(WtGhostty *g, WtEffectFn effect, void *userdata) {
  g->effect = effect;
  g->userdata = userdata;
}

void wt_ghostty_write(WtGhostty *g, const uint8_t *data, size_t len) {
  ghostty_terminal_vt_write(g->terminal, data, len);
}

int wt_ghostty_resize(WtGhostty *g, uint16_t cols, uint16_t rows,
                      uint32_t cell_w, uint32_t cell_h) {
  return ghostty_terminal_resize(g->terminal, cols, rows, cell_w, cell_h);
}

void wt_ghostty_size(WtGhostty *g, uint16_t *cols, uint16_t *rows) {
  ghostty_terminal_get(g->terminal, GHOSTTY_TERMINAL_DATA_COLS, cols);
  ghostty_terminal_get(g->terminal, GHOSTTY_TERMINAL_DATA_ROWS, rows);
}

void wt_ghostty_reset(WtGhostty *g) { ghostty_terminal_reset(g->terminal); }

int wt_ghostty_set_scrollback_lines(WtGhostty *g, size_t lines) {
  return ghostty_terminal_set(g->terminal,
                              GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &lines);
}

/* where: 0 = top, 1 = bottom, 2 = by `delta` rows (up is negative). */
void wt_ghostty_scroll(WtGhostty *g, int where, intptr_t delta) {
  GhosttyTerminalScrollViewport s = {0};
  switch (where) {
    case 0: s.tag = GHOSTTY_SCROLL_VIEWPORT_TOP; break;
    case 1: s.tag = GHOSTTY_SCROLL_VIEWPORT_BOTTOM; break;
    default:
      s.tag = GHOSTTY_SCROLL_VIEWPORT_DELTA;
      s.value.delta = delta;
      break;
  }
  ghostty_terminal_scroll_viewport(g->terminal, s);
}

void wt_ghostty_free_bytes(uint8_t *ptr, size_t len) {
  ghostty_free(NULL, ptr, len);
}

/* Plain text of the active screen, scrollback included; free the result
 * with wt_ghostty_free_bytes. */
int wt_ghostty_screen_text(WtGhostty *g, uint8_t **out, size_t *len) {
  GhosttyFormatterTerminalOptions opts =
      GHOSTTY_INIT_SIZED(GhosttyFormatterTerminalOptions);
  opts.emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
  opts.unwrap = true;
  opts.trim = true;
  GhosttyFormatter f = NULL;
  int r = ghostty_formatter_terminal_new(NULL, &f, g->terminal, opts);
  if (r != GHOSTTY_SUCCESS) return r;
  r = ghostty_formatter_format_alloc(f, NULL, out, len);
  ghostty_formatter_free(f);
  return r;
}

/* Plain text of the terminal's selection; GHOSTTY_NO_VALUE when none. */
int wt_ghostty_selection_text(WtGhostty *g, uint8_t **out, size_t *len) {
  GhosttyTerminalSelectionFormatOptions opts =
      GHOSTTY_INIT_SIZED(GhosttyTerminalSelectionFormatOptions);
  opts.emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
  opts.unwrap = true;
  opts.trim = true;
  opts.selection = NULL;
  *out = NULL;
  *len = 0;
  return ghostty_terminal_selection_format_alloc(g->terminal, NULL, opts, out,
                                                 len);
}

int wt_ghostty_select_all(WtGhostty *g) {
  GhosttySelection sel = GHOSTTY_INIT_SIZED(GhosttySelection);
  int r = ghostty_terminal_select_all(g->terminal, &sel);
  if (r != GHOSTTY_SUCCESS) return r;
  return ghostty_terminal_set(g->terminal, GHOSTTY_TERMINAL_OPT_SELECTION,
                              &sel);
}

/* Select from one viewport cell to another (inclusive), as a mouse drag. */
int wt_ghostty_select_viewport(WtGhostty *g, uint16_t x0, uint16_t y0,
                               uint16_t x1, uint16_t y1) {
  GhosttySelection sel = GHOSTTY_INIT_SIZED(GhosttySelection);
  GhosttyPoint a = {.tag = GHOSTTY_POINT_TAG_VIEWPORT,
                    .value = {.coordinate = {.x = x0, .y = y0}}};
  GhosttyPoint b = {.tag = GHOSTTY_POINT_TAG_VIEWPORT,
                    .value = {.coordinate = {.x = x1, .y = y1}}};
  sel.start = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  sel.end = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  int r = ghostty_terminal_grid_ref(g->terminal, a, &sel.start);
  if (r != GHOSTTY_SUCCESS) return r;
  r = ghostty_terminal_grid_ref(g->terminal, b, &sel.end);
  if (r != GHOSTTY_SUCCESS) return r;
  return ghostty_terminal_set(g->terminal, GHOSTTY_TERMINAL_OPT_SELECTION,
                              &sel);
}

void wt_ghostty_clear_selection(WtGhostty *g) {
  ghostty_terminal_set(g->terminal, GHOSTTY_TERMINAL_OPT_SELECTION, NULL);
}

/* Encode pasted text for the terminal's current mode (bracketed paste or
 * not) into `out`. GHOSTTY_OUT_OF_SPACE reports the size needed in
 * `written`. `text` is modified in place (unsafe control bytes). */
int wt_ghostty_encode_paste(WtGhostty *g, char *text, size_t len, char *out,
                            size_t cap, size_t *written) {
  GhosttyTerminalModeConfig mode = {.mode = GHOSTTY_MODE_BRACKETED_PASTE};
  bool bracketed = false;
  if (ghostty_terminal_get(g->terminal, GHOSTTY_TERMINAL_DATA_MODE, &mode) ==
      GHOSTTY_SUCCESS)
    bracketed = mode.value;
  return ghostty_paste_encode(text, len, bracketed, out, cap, written);
}

/* Whether DEC private mode `value` is set (9, 1000, 1004, 1049, …). */
bool wt_ghostty_dec_mode(WtGhostty *g, uint16_t value) {
  GhosttyTerminalModeConfig mode = {.mode = ghostty_mode_new(value, false)};
  return ghostty_terminal_get(g->terminal, GHOSTTY_TERMINAL_DATA_MODE,
                              &mode) == GHOSTTY_SUCCESS &&
         mode.value;
}

/* Encode a focus change; nothing (written = 0) unless the program enabled
 * focus events (mode 1004). */
int wt_ghostty_encode_focus(WtGhostty *g, bool gained, char *out, size_t cap,
                            size_t *written) {
  *written = 0;
  if (!wt_ghostty_dec_mode(g, 1004)) return GHOSTTY_SUCCESS;
  return ghostty_focus_encode(gained ? GHOSTTY_FOCUS_GAINED : GHOSTTY_FOCUS_LOST,
                              out, cap, written);
}

static GhosttyMods mods_from_wt(uint32_t mods) {
  GhosttyMods m = 0;
  if (mods & WT_MOD_SHIFT) m |= GHOSTTY_MODS_SHIFT;
  if (mods & WT_MOD_CTRL) m |= GHOSTTY_MODS_CTRL;
  if (mods & WT_MOD_ALT) m |= GHOSTTY_MODS_ALT;
  if (mods & WT_MOD_SUPER) m |= GHOSTTY_MODS_SUPER;
  return m;
}

/* Encode one mouse event for the terminal's current tracking mode and
 * format (X10, SGR, …); nothing (written = 0) when the mode does not
 * report it. Pixels right of and below the grid count as padding. */
int wt_ghostty_encode_mouse(WtGhostty *g, const WtMouse *mouse, char *out,
                            size_t cap, size_t *written) {
  uint16_t cols = 1, rows = 1;
  wt_ghostty_size(g, &cols, &rows);
  uint32_t cell_w = mouse->cell_w ? mouse->cell_w : 1;
  uint32_t cell_h = mouse->cell_h ? mouse->cell_h : 1;
  uint32_t grid_w = cell_w * cols, grid_h = cell_h * rows;
  GhosttyMouseEncoderSize size = {
      .size = sizeof(GhosttyMouseEncoderSize),
      .screen_width = mouse->width > grid_w ? mouse->width : grid_w,
      .screen_height = mouse->height > grid_h ? mouse->height : grid_h,
      .cell_width = cell_w,
      .cell_height = cell_h,
  };
  size.padding_right = size.screen_width - grid_w;
  size.padding_bottom = size.screen_height - grid_h;
  bool any_pressed = mouse->any_pressed != 0;

  ghostty_mouse_encoder_setopt_from_terminal(g->mouse_encoder, g->terminal);
  ghostty_mouse_encoder_setopt(g->mouse_encoder, GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                               &size);
  ghostty_mouse_encoder_setopt(g->mouse_encoder,
                               GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                               &any_pressed);

  GhosttyMouseAction action = GHOSTTY_MOUSE_ACTION_PRESS;
  if (mouse->action == WT_MOUSE_RELEASE) action = GHOSTTY_MOUSE_ACTION_RELEASE;
  if (mouse->action == WT_MOUSE_MOTION) action = GHOSTTY_MOUSE_ACTION_MOTION;
  ghostty_mouse_event_set_action(g->mouse_event, action);
  switch (mouse->button) {
    case WT_BUTTON_LEFT:
      ghostty_mouse_event_set_button(g->mouse_event, GHOSTTY_MOUSE_BUTTON_LEFT);
      break;
    case WT_BUTTON_WHEEL_UP:
      ghostty_mouse_event_set_button(g->mouse_event, GHOSTTY_MOUSE_BUTTON_FOUR);
      break;
    case WT_BUTTON_WHEEL_DOWN:
      ghostty_mouse_event_set_button(g->mouse_event, GHOSTTY_MOUSE_BUTTON_FIVE);
      break;
    default: ghostty_mouse_event_clear_button(g->mouse_event); break;
  }
  ghostty_mouse_event_set_mods(g->mouse_event, mods_from_wt(mouse->mods));
  ghostty_mouse_event_set_position(
      g->mouse_event, (GhosttyMousePosition){.x = mouse->x, .y = mouse->y});
  return ghostty_mouse_encoder_encode(g->mouse_encoder, g->mouse_event, out,
                                      cap, written);
}

/* X11/GDK keysym → physical key, for the keys the encoder treats
 * specially. Printable keys outside this table go out as their text. */
static GhosttyKey key_from_keysym(uint32_t sym) {
  if (sym >= 'a' && sym <= 'z') return GHOSTTY_KEY_A + (sym - 'a');
  if (sym >= 'A' && sym <= 'Z') return GHOSTTY_KEY_A + (sym - 'A');
  if (sym >= '0' && sym <= '9') return GHOSTTY_KEY_DIGIT_0 + (sym - '0');
  if (sym >= 0xffbe && sym <= 0xffc9) return GHOSTTY_KEY_F1 + (sym - 0xffbe);
  switch (sym) {
    case ' ': return GHOSTTY_KEY_SPACE;
    case '-': return GHOSTTY_KEY_MINUS;
    case '=': return GHOSTTY_KEY_EQUAL;
    case '[': return GHOSTTY_KEY_BRACKET_LEFT;
    case ']': return GHOSTTY_KEY_BRACKET_RIGHT;
    case '\\': return GHOSTTY_KEY_BACKSLASH;
    case ';': return GHOSTTY_KEY_SEMICOLON;
    case '\'': return GHOSTTY_KEY_QUOTE;
    case ',': return GHOSTTY_KEY_COMMA;
    case '.': return GHOSTTY_KEY_PERIOD;
    case '/': return GHOSTTY_KEY_SLASH;
    case '`': return GHOSTTY_KEY_BACKQUOTE;
    case 0xff08: return GHOSTTY_KEY_BACKSPACE;
    case 0xff09: case 0xfe20: return GHOSTTY_KEY_TAB;
    case 0xff0d: return GHOSTTY_KEY_ENTER;
    case 0xff8d: return GHOSTTY_KEY_NUMPAD_ENTER;
    case 0xff1b: return GHOSTTY_KEY_ESCAPE;
    case 0xffff: return GHOSTTY_KEY_DELETE;
    case 0xff63: return GHOSTTY_KEY_INSERT;
    case 0xff50: return GHOSTTY_KEY_HOME;
    case 0xff57: return GHOSTTY_KEY_END;
    case 0xff55: return GHOSTTY_KEY_PAGE_UP;
    case 0xff56: return GHOSTTY_KEY_PAGE_DOWN;
    case 0xff51: return GHOSTTY_KEY_ARROW_LEFT;
    case 0xff52: return GHOSTTY_KEY_ARROW_UP;
    case 0xff53: return GHOSTTY_KEY_ARROW_RIGHT;
    case 0xff54: return GHOSTTY_KEY_ARROW_DOWN;
    default: return GHOSTTY_KEY_UNIDENTIFIED;
  }
}

/* Encode one key press for the terminal's current modes (cursor keys,
 * Kitty keyboard flags, …). */
int wt_ghostty_encode_key(WtGhostty *g, const WtKey *key, char *out,
                          size_t cap, size_t *written) {
  GhosttyMods m = mods_from_wt(key->mods);

  ghostty_key_encoder_setopt_from_terminal(g->encoder, g->terminal);
  ghostty_key_event_set_action(g->key_event, GHOSTTY_KEY_ACTION_PRESS);
  ghostty_key_event_set_key(g->key_event, key_from_keysym(key->keysym));
  ghostty_key_event_set_mods(g->key_event, m);
  ghostty_key_event_set_consumed_mods(g->key_event, 0);
  ghostty_key_event_set_composing(g->key_event, false);
  ghostty_key_event_set_utf8(g->key_event, key->utf8, key->utf8_len);
  ghostty_key_event_set_unshifted_codepoint(g->key_event, key->unshifted);
  return ghostty_key_encoder_encode(g->encoder, g->key_event, out, cap,
                                    written);
}

/* ABI checks for the Rust mirrors (ffi.rs tests). */
size_t wt_ghostty_cell_size(void) { return sizeof(WtCell); }
size_t wt_ghostty_frame_size(void) { return sizeof(WtFrame); }

static void rgb(uint8_t out[3], GhosttyColorRgb c) {
  out[0] = c.r;
  out[1] = c.g;
  out[2] = c.b;
}

/* Snapshot the viewport into `cells` (row-major, frame->cols per row).
 * GHOSTTY_OUT_OF_SPACE when `cap` < cols * rows; `frame` is filled first,
 * so the caller can grow the buffer and retry. */
int wt_ghostty_snapshot(WtGhostty *g, WtFrame *frame, WtCell *cells,
                        size_t cap) {
  int r = ghostty_render_state_update(g->render, g->terminal);
  if (r != GHOSTTY_SUCCESS) return r;

  GhosttyRenderStateColors colors =
      GHOSTTY_INIT_SIZED(GhosttyRenderStateColors);
  GhosttyRenderStateCursor cursor =
      GHOSTTY_INIT_SIZED(GhosttyRenderStateCursor);
  ghostty_render_state_get(g->render, GHOSTTY_RENDER_STATE_DATA_COLS,
                           &frame->cols);
  ghostty_render_state_get(g->render, GHOSTTY_RENDER_STATE_DATA_ROWS,
                           &frame->rows);
  ghostty_render_state_get(g->render, GHOSTTY_RENDER_STATE_DATA_COLORS,
                           &colors);
  ghostty_render_state_get(g->render, GHOSTTY_RENDER_STATE_DATA_CURSOR,
                           &cursor);
  rgb(frame->fg, colors.foreground);
  rgb(frame->bg, colors.background);
  rgb(frame->cursor_rgb, colors.cursor);
  frame->cursor_has_rgb = colors.cursor_has_value;
  frame->cursor_visible = cursor.visible && cursor.viewport_has_value;
  frame->cursor_x = cursor.viewport_x;
  frame->cursor_y = cursor.viewport_y;

  size_t cols = frame->cols;
  if (cap < cols * (size_t)frame->rows) return GHOSTTY_OUT_OF_SPACE;
  memset(cells, 0, sizeof(WtCell) * cols * frame->rows);

  r = ghostty_render_state_get(g->render, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                               &g->rows);
  if (r != GHOSTTY_SUCCESS) return r;
  size_t y = 0;
  while (y < frame->rows && ghostty_render_state_row_iterator_next(g->rows)) {
    if (ghostty_render_state_row_get(g->rows, GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                                     &g->cells) != GHOSTTY_SUCCESS) {
      y++;
      continue;
    }
    size_t x = 0;
    while (x < cols && ghostty_render_state_row_cells_next(g->cells)) {
      WtCell *cell = &cells[y * cols + x];
      uint32_t len = 0;
      ghostty_render_state_row_cells_get(
          g->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN, &len);
      if (len > 0) {
        /* Base codepoint only; combining marks are not drawn. */
        uint32_t cps[16];
        if (len <= 16 &&
            ghostty_render_state_row_cells_get(
                g->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                cps) == GHOSTTY_SUCCESS)
          cell->codepoint = cps[0];
      }
      GhosttyColorRgb c;
      if (ghostty_render_state_row_cells_get(
              g->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR, &c) ==
          GHOSTTY_SUCCESS)
        rgb(cell->fg, c);
      else
        rgb(cell->fg, colors.foreground);
      if (ghostty_render_state_row_cells_get(
              g->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR, &c) ==
          GHOSTTY_SUCCESS) {
        rgb(cell->bg, c);
        cell->has_bg = 1;
      } else {
        rgb(cell->bg, colors.background);
      }
      bool styled = false;
      ghostty_render_state_row_cells_get(
          g->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_HAS_STYLING, &styled);
      if (styled) {
        GhosttyStyle style = GHOSTTY_INIT_SIZED(GhosttyStyle);
        if (ghostty_render_state_row_cells_get(
                g->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE, &style) ==
            GHOSTTY_SUCCESS) {
          if (style.bold) cell->flags |= WT_CELL_BOLD;
          if (style.italic) cell->flags |= WT_CELL_ITALIC;
          if (style.underline != GHOSTTY_SGR_UNDERLINE_NONE)
            cell->flags |= WT_CELL_UNDERLINE;
          if (style.strikethrough) cell->flags |= WT_CELL_STRIKE;
          if (style.inverse) cell->flags |= WT_CELL_INVERSE;
          if (style.invisible) cell->flags |= WT_CELL_INVISIBLE;
          if (style.faint) cell->flags |= WT_CELL_FAINT;
        }
      }
      bool selected = false;
      ghostty_render_state_row_cells_get(
          g->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_SELECTED, &selected);
      if (selected) cell->flags |= WT_CELL_SELECTED;
      x++;
    }
    y++;
  }
  ghostty_render_state_clean(g->render);
  return GHOSTTY_SUCCESS;
}
