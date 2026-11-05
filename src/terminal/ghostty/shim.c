/*
 * A narrow C shim over the pinned libghostty-vt C API (doc 02
 * §GhosttyBackend, pin in §libghostty-vt pin).
 *
 * libghostty-vt's API is built on sized structs and tagged unions. Their
 * layouts come from the C compiler here, so the Rust side (`ffi.rs`) only
 * declares the plain `wt_ghostty_*` functions below and a few flat structs.
 * Compiled by build.rs only with the `backend-ghostty` feature.
 */

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include <ghostty/vt.h>

/* Effect kinds delivered to the Rust callback (mirrored in ffi.rs). */
#define WT_EFFECT_PTY 1   /* bytes the terminal answers with (DA, DSR, …) */
#define WT_EFFECT_BELL 2
#define WT_EFFECT_TITLE 3 /* the new title, UTF-8 */
#define WT_EFFECT_PWD 4   /* the reported directory, raw (OSC 7: a URI) */

/* Cell flags (mirrored in ffi.rs). */
#define WT_CELL_BOLD (1u << 0)
#define WT_CELL_ITALIC (1u << 1)
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
#define WT_BUTTON_RIGHT 2
#define WT_BUTTON_MIDDLE 3
#define WT_BUTTON_WHEEL_UP 4
#define WT_BUTTON_WHEEL_DOWN 5
#define WT_BUTTON_WHEEL_LEFT 6
#define WT_BUTTON_WHEEL_RIGHT 7

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
  uint32_t codepoint; /* the grapheme's base codepoint; 0 = empty */
  uint8_t fg[3];
  uint8_t bg[3];
  uint8_t underline_rgb[3];
  uint8_t flags;
  uint8_t has_bg;            /* 0 = default background */
  uint8_t underline_has_rgb; /* 0 = the line takes the text color */
  uint8_t underline;         /* GHOSTTY_SGR_UNDERLINE_*; 0 = none */
  uint16_t grapheme_len; /* codepoints in the cluster, base included:
                            0 = empty, 1 = just `codepoint` */
  uint16_t link;         /* 0 = no OSC 8 hyperlink, else its entry, from 1,
                            in the snapshot's links (wt_ghostty_links) */
  uint32_t grapheme;     /* with grapheme_len > 1: where the whole cluster
                            starts in the snapshot's grapheme buffer */
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
  uint8_t cursor_style; /* GhosttyRenderStateCursorVisualStyle */
  uint8_t cursor_blinking;
} WtFrame;

/* One viewport row's run of search highlight, cells [start, end). */
typedef struct {
  uint16_t row;
  uint16_t start;
  uint16_t end;
  uint8_t current; /* part of the current match */
} WtSpan;

typedef struct {
  GhosttyTerminal terminal;
  GhosttyRenderState render;
  GhosttyRenderStateRowIterator rows;
  GhosttyRenderStateRowCells cells;
  GhosttyKeyEncoder encoder;
  GhosttyKeyEvent key_event;
  GhosttyMouseEncoder mouse_encoder;
  GhosttyMouseEvent mouse_event;
  GhosttySelectionGesture gesture;
  WtEffectFn effect;
  void *userdata;
  /* Search: created on first use; idle while `needle` is NULL. */
  GhosttySearch search;
  uint8_t *needle; /* the query as typed, compared exactly */
  size_t needle_len;
  bool needle_cased; /* has ASCII letters, which libghostty-vt folds */
  uint8_t *scratch;  /* needle_len bytes, for reading a match back */
  /* The current match, tracked so it follows the terminal's changes. */
  bool found;
  GhosttyTrackedGridRef found_first;
  GhosttyTrackedGridRef found_last;
  GhosttyTerminalScreen found_screen;
  GhosttySelection *matches; /* reused buffer for match lists */
  size_t matches_cap;
  /* The last snapshot's OSC 8 links, the buffers reused frame to frame:
   * entry n, from 1, is link_uris up to link_ends[n - 1], starting where
   * the entry before it ends. */
  uint8_t *link_uris;
  size_t link_uris_cap;
  size_t link_uris_len;
  size_t *link_ends;
  size_t link_ends_cap;
  size_t link_count;
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

static void emit_string(GhosttyTerminal t, WtGhostty *g,
                        GhosttyTerminalData data, int kind) {
  GhosttyString value = {0};
  if (ghostty_terminal_get(t, data, &value) != GHOSTTY_SUCCESS) return;
  if (g->effect)
    g->effect(g->userdata, kind, (const uint8_t *)value.ptr, value.len);
}

static void on_title(GhosttyTerminal t, void *ud) {
  emit_string(t, ud, GHOSTTY_TERMINAL_DATA_TITLE, WT_EFFECT_TITLE);
}

static void on_pwd(GhosttyTerminal t, void *ud) {
  emit_string(t, ud, GHOSTTY_TERMINAL_DATA_PWD, WT_EFFECT_PWD);
}

void wt_ghostty_search_clear(WtGhostty *g);

void wt_ghostty_free(WtGhostty *g) {
  if (!g) return;
  wt_ghostty_search_clear(g);
  ghostty_search_free(g->search);
  free(g->matches);
  free(g->link_uris);
  free(g->link_ends);
  ghostty_mouse_event_free(g->mouse_event);
  ghostty_mouse_encoder_free(g->mouse_encoder);
  ghostty_key_event_free(g->key_event);
  ghostty_key_encoder_free(g->encoder);
  ghostty_render_state_row_cells_free(g->cells);
  ghostty_render_state_row_iterator_free(g->rows);
  ghostty_render_state_free(g->render);
  ghostty_selection_gesture_free(g->gesture, g->terminal);
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
      ghostty_mouse_event_new(NULL, &g->mouse_event) != GHOSTTY_SUCCESS ||
      ghostty_selection_gesture_new(NULL, &g->gesture) != GHOSTTY_SUCCESS) {
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
  ghostty_terminal_set(g->terminal, GHOSTTY_TERMINAL_OPT_PWD_CHANGED,
                       (const void *)on_pwd);
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

/* A full reset also ends the search: there is nothing left to find. */
void wt_ghostty_reset(WtGhostty *g) {
  wt_ghostty_search_clear(g);
  ghostty_selection_gesture_reset(g->gesture, g->terminal);
  ghostty_terminal_reset(g->terminal);
}

/* The line limit decides: left in place, the default byte limit is reached
 * first. Pruning goes by page, so only a byte limit of zero keeps nothing. */
int wt_ghostty_set_scrollback_lines(WtGhostty *g, size_t lines) {
  size_t none = 0;
  int r = ghostty_terminal_set(g->terminal,
                               GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                               lines == 0 ? &none : NULL);
  if (r != GHOSTTY_SUCCESS) return r;
  return ghostty_terminal_set(g->terminal,
                              GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &lines);
}

static GhosttyColorRgb rgb_in(const uint8_t c[3]) {
  return (GhosttyColorRgb){.r = c[0], .g = c[1], .b = c[2]};
}

/* Make a color scheme the terminal's default colors: `fg`, `bg` and
 * `cursor` are RGB triples, `ansi` the 16 ANSI colors as 16 of them.
 * Indices 16-255 keep the built-in palette's xterm color cube and
 * grayscale ramp. Colors a program set (OSC 4, 10, 11, 12) stay in force;
 * the next snapshot resolves everything else through the new colors. */
int wt_ghostty_set_colors(WtGhostty *g, const uint8_t *fg, const uint8_t *bg,
                          const uint8_t *cursor, const uint8_t *ansi) {
  GhosttyColorRgb palette[256];
  ghostty_color_palette_default(palette);
  for (size_t i = 0; i < 16; i++) palette[i] = rgb_in(ansi + 3 * i);
  GhosttyColorRgb f = rgb_in(fg), b = rgb_in(bg), c = rgb_in(cursor);
  int r = ghostty_terminal_set(g->terminal, GHOSTTY_TERMINAL_OPT_COLOR_PALETTE,
                               palette);
  if (r == GHOSTTY_SUCCESS)
    r = ghostty_terminal_set(g->terminal,
                             GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &f);
  if (r == GHOSTTY_SUCCESS)
    r = ghostty_terminal_set(g->terminal,
                             GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &b);
  if (r == GHOSTTY_SUCCESS)
    r = ghostty_terminal_set(g->terminal, GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
                             &c);
  return r;
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

void wt_ghostty_clear_selection(WtGhostty *g) {
  ghostty_terminal_set(g->terminal, GHOSTTY_TERMINAL_OPT_SELECTION, NULL);
}

/* The selection gesture counts clicks and picks cell, word or line
 * selection; it only computes a selection, so each result is installed
 * here. Having none clears the terminal's, as Ghostty's Surface does. */
static int gesture_event(WtGhostty *g, GhosttySelectionGestureEventType type,
                         uint16_t col, uint16_t row,
                         GhosttySelectionGestureEvent *out) {
  GhosttyPoint point = {.tag = GHOSTTY_POINT_TAG_VIEWPORT,
                        .value = {.coordinate = {.x = col, .y = row}}};
  GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  int r = ghostty_terminal_grid_ref(g->terminal, point, &ref);
  if (r != GHOSTTY_SUCCESS) return r;
  r = ghostty_selection_gesture_event_new(NULL, out, type);
  if (r != GHOSTTY_SUCCESS) return r;
  ghostty_selection_gesture_event_set(*out,
                                      GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                                      &ref);
  return GHOSTTY_SUCCESS;
}

static int gesture_apply(WtGhostty *g, GhosttySelectionGestureEvent event) {
  GhosttySelection sel = GHOSTTY_INIT_SIZED(GhosttySelection);
  int r = ghostty_selection_gesture_event(g->gesture, g->terminal, event, &sel);
  ghostty_selection_gesture_event_free(event);
  if (r == GHOSTTY_NO_VALUE) {
    wt_ghostty_clear_selection(g);
    return GHOSTTY_SUCCESS;
  }
  if (r != GHOSTTY_SUCCESS) return r;
  return ghostty_terminal_set(g->terminal, GHOSTTY_TERMINAL_OPT_SELECTION,
                              &sel);
}

int wt_ghostty_selection_press(WtGhostty *g, uint16_t col, uint16_t row,
                               double x, double y, uint64_t time_ns,
                               uint64_t repeat_ns, double repeat_px) {
  GhosttySelectionGestureEvent event = NULL;
  int r = gesture_event(g, GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS, col, row,
                        &event);
  if (r != GHOSTTY_SUCCESS) return r;
  GhosttySurfacePosition at = {.x = x, .y = y};
  ghostty_selection_gesture_event_set(
      event, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION, &at);
  ghostty_selection_gesture_event_set(
      event, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_TIME_NS, &time_ns);
  ghostty_selection_gesture_event_set(
      event, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_INTERVAL_NS,
      &repeat_ns);
  ghostty_selection_gesture_event_set(
      event, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_DISTANCE, &repeat_px);
  return gesture_apply(g, event);
}

int wt_ghostty_selection_drag(WtGhostty *g, uint16_t col, uint16_t row,
                              double x, double y, uint32_t cell_w,
                              uint32_t height) {
  GhosttySelectionGestureEvent event = NULL;
  int r = gesture_event(g, GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DRAG, col, row,
                        &event);
  if (r != GHOSTTY_SUCCESS) return r;
  uint16_t cols = 0, rows = 0;
  wt_ghostty_size(g, &cols, &rows);
  GhosttySurfacePosition at = {.x = x, .y = y};
  GhosttySelectionGestureGeometry geometry = {
      .columns = cols, .cell_width = cell_w, .screen_height = height};
  ghostty_selection_gesture_event_set(
      event, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION, &at);
  ghostty_selection_gesture_event_set(
      event, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY, &geometry);
  return gesture_apply(g, event);
}

/* A release keeps the click count, for the next press to continue. */
void wt_ghostty_selection_release(WtGhostty *g, uint16_t col, uint16_t row) {
  GhosttySelectionGestureEvent event = NULL;
  if (gesture_event(g, GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_RELEASE, col, row,
                    &event) != GHOSTTY_SUCCESS)
    return;
  ghostty_selection_gesture_event(g->gesture, g->terminal, event, NULL);
  ghostty_selection_gesture_event_free(event);
}

void wt_ghostty_selection_cancel(WtGhostty *g) {
  ghostty_selection_gesture_reset(g->gesture, g->terminal);
}

/* Search finds matches with a GhosttySearch over the active screen,
 * scrollback included, driven on the terminal's thread like every call
 * here. Two things are done on top of it:
 *
 * - libghostty-vt matches ASCII letters case-insensitively and VTE does
 *   not, so a match is read back and compared with the needle as typed.
 * - The current match lives in tracked grid refs, not in the search's own
 *   selected match: moving that takes one call per match, each as slow as
 *   the match list is long, and a rare exact match among many that differ
 *   in case would freeze the UI for seconds.
 *
 * Neither touches the terminal's selection. */

/* End the search: the needle goes, the search idles, nothing is lit. */
void wt_ghostty_search_clear(WtGhostty *g) {
  if (g->search)
    ghostty_search_set(g->search, GHOSTTY_SEARCH_OPT_NEEDLE, NULL);
  ghostty_tracked_grid_ref_free(g->found_first);
  ghostty_tracked_grid_ref_free(g->found_last);
  free(g->needle);
  free(g->scratch);
  g->found_first = NULL;
  g->found_last = NULL;
  g->needle = NULL;
  g->scratch = NULL;
  g->needle_len = 0;
  g->needle_cased = false;
  g->found = false;
}

static int set_needle(WtGhostty *g, const uint8_t *needle, size_t len) {
  uint8_t *copy = malloc(len), *scratch = malloc(len);
  if (!copy || !scratch) {
    free(copy);
    free(scratch);
    return GHOSTTY_OUT_OF_MEMORY;
  }
  memcpy(copy, needle, len);
  /* Cleared first, so a needle that differs only in case restarts too. */
  wt_ghostty_search_clear(g);
  GhosttyString value = {.ptr = copy, .len = len};
  int r = ghostty_search_set(g->search, GHOSTTY_SEARCH_OPT_NEEDLE, &value);
  if (r != GHOSTTY_SUCCESS) {
    free(copy);
    free(scratch);
    return r;
  }
  g->needle = copy;
  g->needle_len = len;
  g->scratch = scratch;
  for (size_t i = 0; i < len; i++)
    if ((copy[i] >= 'A' && copy[i] <= 'Z') || (copy[i] >= 'a' && copy[i] <= 'z'))
      g->needle_cased = true;
  return GHOSTTY_SUCCESS;
}

/* Whether a match reads exactly as the needle, case included. */
static bool exact(WtGhostty *g, const GhosttySelection *match) {
  if (!g->needle_cased) return true;
  GhosttyTerminalSelectionFormatOptions opts =
      GHOSTTY_INIT_SIZED(GhosttyTerminalSelectionFormatOptions);
  opts.emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
  opts.unwrap = true;
  opts.trim = false;
  opts.selection = match;
  size_t written = 0;
  return ghostty_terminal_selection_format_buf(g->terminal, opts, g->scratch,
                                               g->needle_len, &written) ==
             GHOSTTY_SUCCESS &&
         written == g->needle_len &&
         memcmp(g->scratch, g->needle, written) == 0;
}

/* A match's first and last cell in screen coordinates (row 0 is the
 * oldest scrollback line), in reading order; `wide` when the last is a
 * wide character, which covers the next cell too. */
typedef struct {
  GhosttyPointCoordinate first;
  GhosttyPointCoordinate last;
  bool wide;
} Extent;

static bool extent(WtGhostty *g, const GhosttySelection *m, Extent *e) {
  GhosttyPointCoordinate a, b;
  if (ghostty_terminal_point_from_grid_ref(g->terminal, &m->start,
                                           GHOSTTY_POINT_TAG_SCREEN,
                                           &a) != GHOSTTY_SUCCESS ||
      ghostty_terminal_point_from_grid_ref(g->terminal, &m->end,
                                           GHOSTTY_POINT_TAG_SCREEN,
                                           &b) != GHOSTTY_SUCCESS)
    return false;
  bool in_order = a.y < b.y || (a.y == b.y && a.x <= b.x);
  e->first = in_order ? a : b;
  e->last = in_order ? b : a;
  GhosttyCell cell = 0;
  GhosttyCellWide wide = GHOSTTY_CELL_WIDE_NARROW;
  e->wide = ghostty_grid_ref_cell(in_order ? &m->end : &m->start, &cell) ==
                GHOSTTY_SUCCESS &&
            ghostty_cell_get(cell, GHOSTTY_CELL_DATA_WIDE, &wide) ==
                GHOSTTY_SUCCESS &&
            wide == GHOSTTY_CELL_WIDE_WIDE;
  return true;
}

static bool same_extent(const Extent *a, const Extent *b) {
  return a->first.x == b->first.x && a->first.y == b->first.y &&
         a->last.x == b->last.x && a->last.y == b->last.y;
}

/* The screen row at the top of the viewport. */
static bool viewport_top(WtGhostty *g, uint32_t *top) {
  GhosttyPoint p = {.tag = GHOSTTY_POINT_TAG_VIEWPORT,
                    .value = {.coordinate = {.x = 0, .y = 0}}};
  GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  GhosttyPointCoordinate c;
  if (ghostty_terminal_grid_ref(g->terminal, p, &ref) != GHOSTTY_SUCCESS ||
      ghostty_terminal_point_from_grid_ref(g->terminal, &ref,
                                           GHOSTTY_POINT_TAG_SCREEN,
                                           &c) != GHOSTTY_SUCCESS)
    return false;
  *top = c.y;
  return true;
}

/* A match list (all, or the viewport's) into g->matches; its length, 0 on
 * failure. Lists run newest first: bottom of the screen up. */
static size_t read_matches(WtGhostty *g, GhosttySearchData data) {
  GhosttySelectionBuffer buf = {.ptr = g->matches, .cap = g->matches_cap};
  int r = ghostty_search_get(g->search, data, &buf);
  if (r == GHOSTTY_OUT_OF_SPACE) {
    GhosttySelection *grown = realloc(g->matches, buf.len * sizeof *grown);
    if (!grown) return 0;
    g->matches = grown;
    g->matches_cap = buf.len;
    buf = (GhosttySelectionBuffer){.ptr = grown, .cap = buf.len};
    r = ghostty_search_get(g->search, data, &buf);
  }
  return r == GHOSTTY_SUCCESS ? buf.len : 0;
}

/* The current match, if it is still there on the active screen. */
static bool current_match(WtGhostty *g, GhosttySelection *match) {
  GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
  if (!g->found ||
      ghostty_terminal_get(g->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                           &screen) != GHOSTTY_SUCCESS ||
      screen != g->found_screen)
    return false;
  *match = GHOSTTY_INIT_SIZED(GhosttySelection);
  match->start = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  match->end = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  return ghostty_tracked_grid_ref_snapshot(g->found_first, &match->start) ==
             GHOSTTY_SUCCESS &&
         ghostty_tracked_grid_ref_snapshot(g->found_last, &match->end) ==
             GHOSTTY_SUCCESS;
}

static int track(WtGhostty *g, GhosttyTrackedGridRef *ref,
                 GhosttyPointCoordinate at) {
  GhosttyPoint point = {.tag = GHOSTTY_POINT_TAG_SCREEN,
                        .value = {.coordinate = at}};
  if (*ref) return ghostty_tracked_grid_ref_set(*ref, g->terminal, point);
  return ghostty_terminal_grid_ref_track(g->terminal, point, ref);
}

/* Make the match at `e` the current one. */
static int set_current(WtGhostty *g, const Extent *e) {
  g->found = false;
  int r = track(g, &g->found_first, e->first);
  if (r == GHOSTTY_SUCCESS) r = track(g, &g->found_last, e->last);
  if (r == GHOSTTY_SUCCESS)
    r = ghostty_terminal_get(g->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                             &g->found_screen);
  g->found = r == GHOSTTY_SUCCESS;
  return r;
}

/* The first of the `n` matches in g->matches that starts before `at`, or
 * `n`. They are in screen order, so a binary search does. */
static size_t first_before(WtGhostty *g, size_t n, GhosttyPointCoordinate at) {
  size_t lo = 0, hi = n;
  while (lo < hi) {
    size_t mid = lo + (hi - lo) / 2;
    Extent e;
    if (!extent(g, &g->matches[mid], &e) || e.first.y < at.y ||
        (e.first.y == at.y && e.first.x < at.x))
      hi = mid;
    else
      lo = mid + 1;
  }
  return lo;
}

/* The match a search tries first, an index into the `n` in g->matches: the
 * one past the current match or, with none, the first from the viewport,
 * as in VTE: forward from its top, backward from its bottom. Wraps around. */
static size_t start_index(WtGhostty *g, size_t n, bool backward) {
  GhosttyPointCoordinate at = {0};
  GhosttySelection match;
  Extent e;
  uint32_t top = 0;
  uint16_t cols = 0, rows = 0;
  if (current_match(g, &match) && extent(g, &match, &e)) {
    at = e.first;
    if (!backward) at.x++;
  } else if (viewport_top(g, &top)) {
    wt_ghostty_size(g, &cols, &rows);
    at.y = backward ? top + rows : top;
  }
  size_t i = first_before(g, n, at);
  if (backward) return i < n ? i : 0;
  return i > 0 ? i - 1 : n - 1;
}

/* Scroll a match the viewport does not show in full to its middle. */
static void reveal(WtGhostty *g, const Extent *e) {
  uint32_t top = 0;
  uint16_t cols = 0, rows = 0;
  if (!viewport_top(g, &top)) return;
  wt_ghostty_size(g, &cols, &rows);
  if (e->first.y >= top && e->last.y < top + rows) return;
  intptr_t middle = ((intptr_t)e->first.y + (intptr_t)e->last.y) / 2;
  wt_ghostty_scroll(g, 2, middle - rows / 2 - (intptr_t)top);
}

/* Make the next match of `needle` (UTF-8 as typed) current, `backward`
 * toward older lines, wrapping around through the scrollback, and scroll
 * it into view. A new needle starts from the viewport; the same one again
 * moves on from its last match. GHOSTTY_NO_VALUE when nothing matches; an
 * empty needle ends the search. */
int wt_ghostty_search(WtGhostty *g, const uint8_t *needle, size_t len,
                      bool backward) {
  if (len == 0) {
    wt_ghostty_search_clear(g);
    return GHOSTTY_NO_VALUE;
  }
  int r;
  if (!g->search) {
    r = ghostty_search_new(NULL, &g->search, g->terminal);
    if (r != GHOSTTY_SUCCESS) return r;
  }
  if (!g->needle || g->needle_len != len ||
      memcmp(g->needle, needle, len) != 0) {
    r = set_needle(g, needle, len);
    if (r != GHOSTTY_SUCCESS) return r;
  }
  r = ghostty_search_run(g->search);
  if (r != GHOSTTY_SUCCESS) return r;

  size_t n = read_matches(g, GHOSTTY_SEARCH_DATA_MATCHES);
  size_t i = n ? start_index(g, n, backward) : 0;
  /* Each match once at most, passing over those that differ in case. */
  for (size_t tried = 0; tried < n; tried++) {
    Extent e;
    if (exact(g, &g->matches[i]) && extent(g, &g->matches[i], &e)) {
      r = set_current(g, &e);
      if (r == GHOSTTY_SUCCESS) reveal(g, &e);
      return r;
    }
    /* The list runs newest first. */
    if (backward)
      i = i + 1 < n ? i + 1 : 0;
    else
      i = i > 0 ? i - 1 : n - 1;
  }
  g->found = false;
  return GHOSTTY_NO_VALUE;
}

/* Add a span per viewport row the match covers. */
static void add_spans(const Extent *e, uint32_t top, uint16_t cols,
                      uint16_t rows, bool current, WtSpan *out, size_t cap,
                      size_t *len) {
  uint32_t y = e->first.y > top ? e->first.y : top;
  for (; y <= e->last.y && y < top + rows; y++) {
    uint32_t end = y == e->last.y ? e->last.x + 1u + e->wide : cols;
    if (*len < cap)
      out[*len] = (WtSpan){
          .row = (uint16_t)(y - top),
          .start = y == e->first.y ? e->first.x : 0,
          .end = (uint16_t)(end < cols ? end : cols),
          .current = current,
      };
    (*len)++;
  }
}

/* The search's highlight on the viewport into `out`: a span per row each
 * match covers, the current match's marked. Feeds the search first, so it
 * follows terminal changes. `len` is the number of spans; when that is
 * more than `cap`, GHOSTTY_OUT_OF_SPACE, and only `cap` were written. */
int wt_ghostty_search_spans(WtGhostty *g, WtSpan *out, size_t cap,
                            size_t *len) {
  *len = 0;
  if (!g->search || !g->needle) return GHOSTTY_SUCCESS;
  int r = ghostty_search_feed(g->search);
  if (r != GHOSTTY_SUCCESS) return r;
  uint32_t top = 0;
  uint16_t cols = 0, rows = 0;
  if (!viewport_top(g, &top)) return GHOSTTY_SUCCESS;
  wt_ghostty_size(g, &cols, &rows);

  /* Text can change under the current match, as under any other. */
  Extent current;
  GhosttySelection match;
  bool has_current = current_match(g, &match) && exact(g, &match) &&
                     extent(g, &match, &current);
  if (has_current) add_spans(&current, top, cols, rows, true, out, cap, len);

  /* The list covers whole pages: skip what is off screen before reading
   * anything back. */
  size_t n = read_matches(g, GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES);
  for (size_t i = 0; i < n; i++) {
    Extent e;
    if (extent(g, &g->matches[i], &e) && e.last.y >= top &&
        e.first.y < top + rows &&
        !(has_current && same_extent(&e, &current)) &&
        exact(g, &g->matches[i]))
      add_spans(&e, top, cols, rows, false, out, cap, len);
  }
  return *len > cap ? GHOSTTY_OUT_OF_SPACE : GHOSTTY_SUCCESS;
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
    case WT_BUTTON_RIGHT:
      ghostty_mouse_event_set_button(g->mouse_event, GHOSTTY_MOUSE_BUTTON_RIGHT);
      break;
    case WT_BUTTON_MIDDLE:
      ghostty_mouse_event_set_button(g->mouse_event,
                                     GHOSTTY_MOUSE_BUTTON_MIDDLE);
      break;
    case WT_BUTTON_WHEEL_UP:
      ghostty_mouse_event_set_button(g->mouse_event, GHOSTTY_MOUSE_BUTTON_FOUR);
      break;
    case WT_BUTTON_WHEEL_DOWN:
      ghostty_mouse_event_set_button(g->mouse_event, GHOSTTY_MOUSE_BUTTON_FIVE);
      break;
    case WT_BUTTON_WHEEL_LEFT:
      ghostty_mouse_event_set_button(g->mouse_event, GHOSTTY_MOUSE_BUTTON_SIX);
      break;
    case WT_BUTTON_WHEEL_RIGHT:
      ghostty_mouse_event_set_button(g->mouse_event, GHOSTTY_MOUSE_BUTTON_SEVEN);
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
size_t wt_ghostty_span_size(void) { return sizeof(WtSpan); }
/* The size alone misses a field moved into the padding. */
size_t wt_ghostty_cell_grapheme_offset(void) {
  return offsetof(WtCell, grapheme);
}
size_t wt_ghostty_cell_grapheme_len_offset(void) {
  return offsetof(WtCell, grapheme_len);
}
size_t wt_ghostty_cell_link_offset(void) { return offsetof(WtCell, link); }
size_t wt_ghostty_cell_underline_offset(void) {
  return offsetof(WtCell, underline);
}
size_t wt_ghostty_cell_underline_rgb_offset(void) {
  return offsetof(WtCell, underline_rgb);
}
size_t wt_ghostty_cell_underline_has_rgb_offset(void) {
  return offsetof(WtCell, underline_has_rgb);
}

static void rgb(uint8_t out[3], GhosttyColorRgb c) {
  out[0] = c.r;
  out[1] = c.g;
  out[2] = c.b;
}

/* Read the URI of the hyperlink at a viewport cell into the free end of
 * link_uris, growing it as needed; its length, 0 if the cell has none. */
static size_t read_link(WtGhostty *g, size_t x, size_t y) {
  GhosttyPoint point = {.tag = GHOSTTY_POINT_TAG_VIEWPORT};
  point.value.coordinate.x = (uint16_t)x;
  point.value.coordinate.y = (uint32_t)y;
  GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  if (ghostty_terminal_grid_ref(g->terminal, point, &ref) != GHOSTTY_SUCCESS)
    return 0;
  size_t start = g->link_uris_len, len = 0;
  int r = ghostty_grid_ref_hyperlink_uri(
      &ref, g->link_uris ? g->link_uris + start : NULL,
      g->link_uris_cap - start, &len);
  if (r == GHOSTTY_OUT_OF_SPACE) {
    size_t cap = (start + len) * 2;
    uint8_t *grown = realloc(g->link_uris, cap);
    if (!grown) return 0;
    g->link_uris = grown;
    g->link_uris_cap = cap;
    r = ghostty_grid_ref_hyperlink_uri(&ref, grown + start, cap - start, &len);
  }
  return r == GHOSTTY_SUCCESS ? len : 0;
}

/* The links entry for the hyperlink at a viewport cell, 0 if it has none.
 * The C API exposes no link id, so a URI is what identifies a link. Cells
 * come in reading order and only the latest entry is compared: a link's
 * cells share one entry, however long the screen's list of links is. */
static uint16_t link_entry(WtGhostty *g, size_t x, size_t y) {
  size_t len = read_link(g, x, y);
  if (len == 0) return 0;
  size_t start = g->link_uris_len;
  if (g->link_count > 0) {
    size_t last = g->link_count > 1 ? g->link_ends[g->link_count - 2] : 0;
    if (start - last == len &&
        memcmp(g->link_uris + last, g->link_uris + start, len) == 0)
      return (uint16_t)g->link_count;
  }
  if (g->link_count == UINT16_MAX) return 0;
  if (g->link_count == g->link_ends_cap) {
    size_t cap = g->link_ends_cap ? g->link_ends_cap * 2 : 16;
    size_t *grown = realloc(g->link_ends, cap * sizeof(size_t));
    if (!grown) return 0;
    g->link_ends = grown;
    g->link_ends_cap = cap;
  }
  g->link_uris_len = start + len;
  g->link_ends[g->link_count++] = g->link_uris_len;
  return (uint16_t)g->link_count;
}

/* The last snapshot's links, valid until the next one: `*count` entries,
 * entry n, from 1, being `uris` up to `ends[n - 1]`, starting where the
 * entry before it ends. Cells point at them by `link`. */
void wt_ghostty_links(WtGhostty *g, const uint8_t **uris, const size_t **ends,
                      size_t *count) {
  *uris = g->link_uris;
  *ends = g->link_ends;
  *count = g->link_count;}

/* A style color in RGB, a palette index through `palette`; false if unset. */
static bool style_rgb(uint8_t out[3], GhosttyStyleColor c,
                      const GhosttyColorRgb *palette) {
  switch (c.tag) {
    case GHOSTTY_STYLE_COLOR_PALETTE:
      rgb(out, palette[c.value.palette]);
      return true;
    case GHOSTTY_STYLE_COLOR_RGB:
      rgb(out, c.value.rgb);
      return true;
    default:
      return false;
  }
}

/* Snapshot the viewport into `cells` (row-major, frame->cols per row),
 * and the whole cluster of each cell holding more than one codepoint into
 * `graphemes`, `*graphemes_len` slots in all. GHOSTTY_OUT_OF_SPACE when
 * `cap` < cols * rows (`frame` is filled first) or the clusters need more
 * than `graphemes_cap` slots (`*graphemes_len` says how many), so the
 * caller can grow either buffer and retry. */
int wt_ghostty_snapshot(WtGhostty *g, WtFrame *frame, WtCell *cells,
                        size_t cap, uint32_t *graphemes, size_t graphemes_cap,
                        size_t *graphemes_len) {
  *graphemes_len = 0;
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
  frame->cursor_style = (uint8_t)cursor.visual_style;
  frame->cursor_blinking = cursor.blinking;

  size_t cols = frame->cols;
  if (cap < cols * (size_t)frame->rows) return GHOSTTY_OUT_OF_SPACE;
  memset(cells, 0, sizeof(WtCell) * cols * frame->rows);

  r = ghostty_render_state_get(g->render, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                               &g->rows);
  if (r != GHOSTTY_SUCCESS) return r;
  size_t used = 0; /* grapheme slots filled, or needed past the cap */
  g->link_uris_len = 0;
  g->link_count = 0;
  size_t y = 0;
  while (y < frame->rows && ghostty_render_state_row_iterator_next(g->rows)) {
    if (ghostty_render_state_row_get(g->rows, GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                                     &g->cells) != GHOSTTY_SUCCESS) {
      y++;
      continue;
    }
    /* May be a false positive; saves every other row the per-cell check. */
    GhosttyRow row;
    bool row_links = false;
    if (ghostty_render_state_row_get(g->rows, GHOSTTY_RENDER_STATE_ROW_DATA_RAW,
                                     &row) == GHOSTTY_SUCCESS)
      ghostty_row_get(row, GHOSTTY_ROW_DATA_HYPERLINK, &row_links);
    size_t x = 0;
    while (x < cols && ghostty_render_state_row_cells_next(g->cells)) {
      WtCell *cell = &cells[y * cols + x];
      /* 0 for an empty cell and for the tail of a wide one: a wide
       * cluster sits whole in its leading cell. */
      uint32_t len = 0;
      ghostty_render_state_row_cells_get(
          g->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN, &len);
      if (len == 1) {
        if (ghostty_render_state_row_cells_get(
                g->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                &cell->codepoint) == GHOSTTY_SUCCESS)
          cell->grapheme_len = 1;
      } else if (len > 1 && len <= UINT16_MAX) {
        /* Past the cap only count, so the caller can grow the buffer. */
        if (used + len <= graphemes_cap &&
            ghostty_render_state_row_cells_get(
                g->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                graphemes + used) == GHOSTTY_SUCCESS) {
          cell->codepoint = graphemes[used];
          cell->grapheme = (uint32_t)used;
          cell->grapheme_len = (uint16_t)len;
        }
        used += len;
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
          cell->underline = (uint8_t)style.underline;
          cell->underline_has_rgb = style_rgb(
              cell->underline_rgb, style.underline_color, colors.palette);
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
      if (row_links) {
        GhosttyCell raw;
        bool linked = false;
        if (ghostty_render_state_row_cells_get(
                g->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW, &raw) ==
                GHOSTTY_SUCCESS &&
            ghostty_cell_get(raw, GHOSTTY_CELL_DATA_HAS_HYPERLINK, &linked) ==
                GHOSTTY_SUCCESS &&
            linked)
          cell->link = link_entry(g, x, y);
      }
      x++;
    }
    y++;
  }
  *graphemes_len = used;
  if (used > graphemes_cap) return GHOSTTY_OUT_OF_SPACE;
  ghostty_render_state_clean(g->render);
  return GHOSTTY_SUCCESS;
}
