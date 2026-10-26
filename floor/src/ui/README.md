# Building a Floor screen

This is the guide for whoever builds the next screen. The Floor is styled with Tailwind utilities on owned components. There
is no hand-written CSS except the entry file, the third-party terminal wrapper and the component lab's entry.

## The rule

Styling is Tailwind utilities on components and screens. Nothing goes into a CSS file, except a token or a keyframe in the
entry file, `src/tailwind.css`.

`tests/stylesheets.test.mjs` enforces it. It lists every stylesheet that may exist, so a new `.css` file fails the test.

## Layers

`src/tailwind.css` declares the order once, and every stylesheet repeats the same first line:

```css
@layer legacy, theme, base, components, utilities;
```

Only the terminal's third-party stylesheet sits in `legacy`, the lowest layer. A rule in a higher layer beats every legacy
rule whatever its selector's specificity, so a utility on an element needs no `!important` and no longer selector. Do not add
`!important`.

Two things follow.

- Tailwind's preflight is not loaded, so elements keep the browser's own styles: a `p`, `ul`, `h3` or `button` has its default
  margin, padding and font until a utility says otherwise. The entry file's `base` layer sets only the page's colour, font and
  scheme on the root, and the body's margin and minimum size.
- Never set `display` on an element with the `popover` attribute. The browser hides a closed popover with `display: none`, and
  an author `display` would show it.

## Tokens

The colour tokens are custom properties on `:root`, for day and night, in `src/tailwind.css`: `--bg`, `--ui-surface`,
`--surface-2`, `--border`, `--border-strong`, `--text`, `--text-muted`, `--text-subtle`, `--accent` (with `-hover`, `-fg` and
`-soft`), `--focus`, and `--ok`, `--warn`, `--danger` and `--info` (each with `-soft`). Night follows the system unless the
shell pins a theme.

The panel colour is `--ui-surface`, not `--surface`, for historical reasons. A few warm-paper values from the first design
remain as tokens because screens still draw them: `--surface-muted`, `--studio-background`, `--portrait-border`, the four
`--terminal-*` colours that the terminal reads, and the `--layer-*` stacking numbers.

Tailwind sees the tokens as theme colours, so a utility uses them by name:

| Token | Utility colour |
| --- | --- |
| `--bg`, `--ui-surface`, `--surface-2`, `--field` | `bg`, `surface`, `surface-2`, `field` |
| `--border`, `--border-strong` | `line`, `line-strong` |
| `--text`, `--text-muted`, `--text-subtle` | `ink`, `muted`, `subtle` |
| `--accent`, `-hover`, `-fg`, `-soft` | `accent`, `accent-hover`, `accent-fg`, `accent-soft` |
| `--focus` | `focus` |
| `--ok`, `--warn`, `--danger`, `--info` (and `-soft`) | `ok`, `warn`, `danger`, `info` (and `-soft`) |

Examples: `bg-surface`, `text-muted`, `border-line`, `hover:bg-surface-2`. Plain CSS reads the same values as `var(--text)`.

The rest of the scale is in the `@theme static` block of the entry file and is also available as custom properties.

- Type: `text-caption` 11, `text-small` 12, `text-base` 13, `text-body` 14, `text-title` 16, `text-page` 20 and `text-display` 24
  pixels. Weights are `font-normal`, `font-medium` and `font-semibold`. Uppercase labels use `tracking-label`.
- Spacing is a 4 pixel step: `p-1` is 4, `gap-2` is 8. Use only 2, 4, 6, 8, 12, 16, 20, 24, 32, 40 and 48 pixels, which is
  `0.5`, `1`, `1.5`, `2`, `3`, `4`, `5`, `6`, `8`, `10` and `12`. A test fails any other spacing class in `src/ui`.
- Radius: `rounded-sm` 4, `rounded-md` 6, `rounded-lg` 8, `rounded-xl` 12, `rounded-pill`.
- Elevation: `shadow-card`, `shadow-pop` and `shadow-sheet`.
- Fonts: `font-sans` is Inter and `font-mono` is JetBrains Mono, both served as files from the build.
- Motion: 120 ms for hover and press, `animate-pop` for 160 ms openings. Reduced motion removes all of it.
- Icons: Lucide, 16 pixels with a 1.75 stroke. Spread `ICON` from `src/ui` onto the icon.

## `cn()`

Build every class string with `cn()` from `src/ui`. It joins class names and drops an earlier utility that a later one
overrides, so a base style and a variant on one element never both reach it.

```tsx
cn('border border-transparent bg-accent', variantClass, className)
```

Without it, `border-transparent` and `border-line-strong` on one element are decided by the order Tailwind happens to emit
them, and a secondary button loses its whole edge. `cn()` knows the Floor's type scale, radii and shadows.

Tailwind reads every file under `src` for class names, so a class only has to appear in a string. Write class names whole:
`'bg-' + colour` is never found.

## Components

Import from `src/ui` (`index.ts`). Each component forwards the native props of the element it draws, merges the `className` you
pass over its own, and takes `ref` as a normal prop.

| Component | Props worth knowing | Hooks it sets |
| --- | --- | --- |
| `Button` | `variant` primary, secondary, ghost or danger; `size` sm or md; `icon`; `loading` | `data-ui="button"`, `data-variant`, `data-size`, `data-loading` |
| `IconButton` | `label` (required by its type), `icon`, `variant`, `size`, `loading` | `data-ui="icon-button"`, `data-variant`, `data-size` |
| `Field` | `label`, `hint`, `error`, `id`; one control inside | `data-ui="field"`, `data-invalid` |
| `Input`, `Textarea` | native props; take id, description and invalid state from a surrounding `Field` | `data-ui="input"`, `data-ui="textarea"` |
| `Select` | `options`, `value`, `onValueChange`, `placeholder`, `name` | `data-ui="select"`, `data-ui="select-list"` |
| `Menu`, `MenuItem`, `MenuSeparator` | `label`, `trigger`, `id`, `align`; items take `onSelect`, `danger`, `icon`, `checked` | `data-ui="menu-button"`, `menu`, `menu-item` |
| `Switch` | `checked`, `onCheckedChange`, `label`, `description`, `busy` | `data-ui="switch"`, `data-state`, `data-part="knob"` |
| `TabList`, `Tab` | `label`; `selected` | `data-ui="tab-list"`, `tab`, `data-state` |
| `Badge` | `state`: needs-you, ready, working, waiting, done or idle | `data-ui="badge"`, `data-state` |
| `Card`, `Panel` | `Panel` takes `title`, `headingId`, `actions` | `data-ui="card"`, `data-ui="panel"` |
| `Dialog` | `size` sm, md or lg; the native element | `data-ui="dialog"`, `data-size` |
| `Toast` | `tone`, `action`, `onDismiss` | `data-ui="toast"`, `data-tone` |
| `Disclosure` | `summary` | `data-ui="disclosure"` |

Keep your own class names at the call site and hook tests on the `data-` attributes or the class, not on utility strings. A
class string changes whenever the design does.

How the pieces behave:

- A `Field` ties its label, hint and error to the control through context, so the call site is
  `<Field label="Name"><Input /></Field>`, with no ids and no spread. Put the context in its own file when you add one, because
  the surface guard treats a context made in the same file as a dynamic tag.
- `Select` and `Menu` use the HTML `popover` attribute and stay inside their own component. They are never portalled to
  `document.body`, so they work inside an open modal `dialog`, where the rest of the page is inert. They set their position
  through the style object model, which the strict page policy allows.
- `Dialog` is the native `dialog`. The caller opens it with `showModal()` and closes it with `close()`, so the browser's focus
  return and the owner's own focus rules stay as they are.
- A `loading` button and a `busy` switch ignore presses but stay in the tab order, so a person using the keyboard does not
  lose their place. A button that is disabled in the usual way uses `disabled`.
- Every focusable component draws the same ring: 2 pixels in `--focus`, shown for keyboard focus only. A control in a box that
  clips uses `RING_INSET`. The ring appears at once; only colours fade.

## The tests that guard the screens

- `tests/ui-components.test.mjs` renders each component to markup and checks its structure and attributes.
- `tests/ui-tokens.test.mjs` pins the token values and checks text at 4.5 to 1, and input edges and the focus ring at 3 to 1.
- `tests/ui-utilities.test.mjs` checks that every class a component writes produces CSS, and that spacing keeps to the
  scale.
- `tests/no-answer-surface.test.mjs` and `tests/focus.test.mjs` read the source for controls. They read each owned component as
  the one native element it draws (`Button` as `button`, `Switch` as `button`, `Select` as `select`). A handler you add at a
  call site is checked and must be admitted there as it would be on the native element.
- `npm run test:browser` is the browser lane, described below.

## The browser lane

`npm run test:browser` builds the component lab in `lab/`, serves it under the Floor's strict page policy (`style-src 'self'`,
no inline script or style, nothing from another origin), and drives it in a real headless Chrome with real key and mouse events.
It checks the menu and the select with the keyboard on the page and inside an open modal dialog (arrows, Home, End, type-ahead,
Escape and focus return), the switch knob, the focus ring on a real Tab walk in both themes, the button states, reduced motion,
and that the page saw no policy violation. It is not part of `npm test`, which has no browser.

It uses the Chrome or Chromium it finds in a usual place. Set `FLOOR_BROWSER` to an executable to use another one, and
`FLOOR_BROWSER_WAIT_MS` to wait longer for the browser to start. When you add a component, add it to `lab/main.tsx` and add a
check to `browser/checks.mjs`. The lab is under `floor/lab`, outside `src`, so no release build contains it.

## Building or changing a screen

1. Build the screen from the components, with utilities for its layout.
2. Put a hook class on an element only when a test, the focus handoff or a script looks it up, and say in the test what the hook is
   for. A class that exists only to style does not belong.
3. Update the tests that read the markup. Prefer a `data-` hook to a class string. `tests/helpers/markup.mjs` holds the helpers
   that take utility classes and component marks out of rendered markup.
4. Run `npm run typecheck`, `npm test` and `npm run test:browser`, and look at the screen in light and dark.
