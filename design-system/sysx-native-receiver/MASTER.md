# Design System Master File

> **LOGIC:** When building a specific page, first check `design-system/pages/[page-name].md`.
> If that file exists, its rules **override** this Master file.
> If not, strictly follow the rules below.

---

**Project:** SysX Native Receiver
**Generated:** 2026-10-15 21:52:00
**Category:** General
**Design Dials:** Variance 3/10 (Focused) | Motion 2/10 (Subtle) | Density 7/10 (Compact)

---

## Global Rules

### Color Palette

| Role | Hex | CSS Variable |
|------|-----|--------------|
| Primary | `#43CFA0` | `--color-primary` |
| On Primary | `#081015` | `--color-on-primary` |
| Secondary | `#17212B` | `--color-secondary` |
| On Secondary | `#EBF1F5` | `--color-on-secondary` |
| Accent/CTA | `#E8B858` | `--color-accent` |
| On Accent/CTA | `#081015` | `--color-on-accent` |
| Background | `#090C10` | `--color-background` |
| Foreground | `#EBF1F5` | `--color-foreground` |
| Card | `#141C25` | `--color-card` |
| Card Foreground | `#EBF1F5` | `--color-card-foreground` |
| Muted | `#141C25` | `--color-muted` |
| Muted Foreground | `#9AA9B4` | `--color-muted-foreground` |
| Border | `#2D3946` | `--color-border` |
| Destructive | `#EF6C6C` | `--color-destructive` |
| On Destructive | `#FFFFFF` | `--color-on-destructive` |
| Ring | `#43CFA0` | `--color-ring` |

**Color Notes:** Near-black blue surfaces; green means live/healthy, amber means waiting/attention, red means failure. No gradients.

### Typography

- **Heading Font:** Fira Code (or native monospace fallback for HUD)
- **Body Font:** Fira Sans (or native sans fallback)
- **Mood:** quiet technical utility, precise, dark, operator-focused
- **Native rule:** monospace is reserved for measured values and status tokens; body copy stays sans-serif.

**CSS Import:**
```css
@import url('https://fonts.googleapis.com/css2?family=Fira+Code:wght@400;500;600;700&family=Fira+Sans:wght@300;400;500;600;700&display=swap');
```

### Spacing Variables

*Density: 7/10 — Standard*

| Token | Value | Usage |
|-------|-------|-------|
| `--space-xs` | `4px` / `0.25rem` | Tight gaps |
| `--space-sm` | `8px` / `0.5rem` | Icon gaps, inline spacing |
| `--space-md` | `16px` / `1rem` | Standard padding |
| `--space-lg` | `24px` / `1.5rem` | Section padding |
| `--space-xl` | `32px` / `2rem` | Large gaps |
| `--space-2xl` | `48px` / `3rem` | Section margins |
| `--space-3xl` | `64px` / `4rem` | Hero padding |

### Shadow Depths

| Level | Value | Usage |
|-------|-------|-------|
| `--shadow-sm` | `0 1px 2px rgba(0,0,0,0.05)` | Subtle lift |
| `--shadow-md` | `0 4px 6px rgba(0,0,0,0.1)` | Cards, buttons |
| `--shadow-lg` | `0 10px 15px rgba(0,0,0,0.1)` | Modals, dropdowns |
| `--shadow-xl` | `0 20px 25px rgba(0,0,0,0.15)` | Hero images, featured cards |

---

## Component Specs

### Buttons

```css
/* Primary Button */
.btn-primary {
  background: #EA580C;
  color: #000000;
  padding: 12px 24px;
  border-radius: 8px;
  font-weight: 600;
  transition: all 200ms ease;
  cursor: pointer;
}

.btn-primary:hover {
  opacity: 0.9;
  transform: translateY(-1px);
}

/* Secondary Button */
.btn-secondary {
  background: transparent;
  color: #2563EB;
  border: 2px solid #2563EB;
  padding: 12px 24px;
  border-radius: 8px;
  font-weight: 600;
  transition: all 200ms ease;
  cursor: pointer;
}
```

### Compact Status Tiles

```css
.card {
  background: #F8FAFC;
  border-radius: 12px;
  padding: 24px;
  box-shadow: var(--shadow-md);
  transition: all 200ms ease;
  cursor: pointer;
}

.card:hover {
  box-shadow: var(--shadow-lg);
  transform: translateY(-2px);
}
```

### Inputs

```css
.input {
  padding: 12px 16px;
  border: 1px solid #E2E8F0;
  border-radius: 8px;
  font-size: 16px;
  transition: border-color 200ms ease;
}

.input:focus {
  border-color: #2563EB;
  outline: none;
  box-shadow: 0 0 0 3px #2563EB20;
}
```

### Modals

```css
.modal-overlay {
  background: rgba(0, 0, 0, 0.5);
  backdrop-filter: blur(4px);
}

.modal {
  background: white;
  border-radius: 16px;
  padding: 32px;
  box-shadow: var(--shadow-xl);
  max-width: 500px;
  width: 90%;
}
```

---

## Style Guidelines

**Style:** Minimalism & Swiss Style

**Keywords:** Clean, simple, spacious, functional, white space, high contrast, geometric, sans-serif, grid-based, essential

**Best For:** Enterprise apps, dashboards, documentation sites, SaaS platforms, professional tools

**Key Effects:** Subtle hover (200-250ms), smooth transitions, sharp shadows if any, clear type hierarchy, fast loading

### Page Pattern

**Pattern Name:** Single-page preview monitor

- **Primary task:** See the current receiver crop immediately and trust the three live measurements.
- **Surface order:** compact title/status bar > large square Preview > in-feed HUD > bottom Mouse State strip.
- **Header (48 px):** SysX mark + name left; right side shows only `● <device>` (receiver status and UDP port were removed by request; the receiver state lives in the Preview empty state). Status is dot + text, never color alone.
- **Preview:** centered column, square ≤640 px, 8 px radius, 1 px border. HUD is one dark plate top-left: `Res: <n>  <fps> FPS  <latency> ms` (the crop is square, so one number; mono values, sans labels/units; latency = per-frame detection time). Object Detection summary sits in a plate bottom-center. Overlays: boxes (class hue / Pixel Seek color), FOV circle, "hairline reticle" target (1.25 px lead fading in from the crop center, 8 px ring with four ticks, 1.6 px core, all on a dark underlay), all mapped after removing the crop origin.
- **Empty state:** `NO FRAME` (mono, muted) above a status headline with dot; a detail line appears only for errors (no "Listening on UDP" line, by request).
- **Mouse State:** one 30 px segmented strip (Left / Right / Middle / M4 / M5, equal widths) matching the Preview width, 7 px radius, hairline separators. Held = green tint, semibold white label and a 2 px green bar on the bottom edge (shape, not only color); released = muted label; no connected device = whole strip at 50 % opacity. Captures: `receiver-monitor-web-mouse-held.png`, `receiver-monitor-web-waiting.png`.
- **Surface:** web page `/monitor.html` (Leptos/WASM, `.monitor` block in `rust-ui/style.css`) shown in a chromeless Edge app window; there is no egui window anymore. CSS tokens: `--m-bg #090C10`, `--m-bar #0F151C`, `--m-feed #05080B`, `--m-cell #121A23`, `--m-border #2D3946`, `--m-text #EBF1F5`, `--m-muted #9AA9B4`, `--m-live #43CFA0`, `--m-wait #E8B858`, `--m-error #EF6C6C`.
- **Sizing:** `--m-side = max(240px, min(640px, 100vw − 40px, 100vh − bar − gap − mouse row − 40px))`; feed and Mouse State share that width.
- **Fonts:** Segoe UI Variable / Segoe UI for text; Cascadia Mono / Consolas only for measurements (HUD values, `NO FRAME`), with tabular numerals.
- **Motion:** only 120–200 ms color transitions on status dots and Mouse State; no pulsing or entrance animation; `prefers-reduced-motion` disables transitions.
- **Captures:** `design-review/receiver-monitor-web-{live,waiting,narrow}.png` (headless Edge, isolated backend, synthetic plugin frames).
- **Removed as decoration/duplication:** corner brackets, "RECEIVER MONITOR" eyebrow, pipeline chip, "LOOPBACK · AUTHENTICATED" caption, and the footer metric strip that repeated the HUD.
- **Navigation:** none inside the native receiver page; Web Config is a separate browser surface.

---

## Motion

**Scroll Reveal** (Subtle) — Trigger: scroll (viewport enter) | Duration: 300-400ms | Easing: `power1.out`

```js
gsap.from(el, { opacity: 0, y: 12, duration: 0.35, ease: 'power1.out', scrollTrigger: { trigger: el, start: 'top 90%', toggleActions: 'play none none reverse' } });
```

**Framework notes:** Requires the ScrollTrigger plugin registered once via gsap.registerPlugin(ScrollTrigger); Use matchMedia('(prefers-reduced-motion: reduce)') to skip non-essential motion and render the final state immediately

- ✅ Keep the y offset small (8-16px) so it reads as a fade, not a slide
- ❌ Don't reveal below-the-fold content needed for SEO/crawlers as invisible-by-default without a no-JS fallback
- ⚡ toggleActions 'play none none reverse' avoids re-triggering on every scroll direction change

---

## Anti-Patterns (Do NOT Use)


### Additional Forbidden Patterns

- ❌ **Emojis as icons** — Use SVG icons (Heroicons, Lucide, Simple Icons)
- ❌ **Missing cursor:pointer** — All clickable elements must have cursor:pointer
- ❌ **Layout-shifting hovers** — Avoid scale transforms that shift layout
- ❌ **Low contrast text** — Maintain 4.5:1 minimum contrast ratio
- ❌ **Instant state changes** — Always use transitions (150-300ms)
- ❌ **Invisible focus states** — Focus states must be visible for a11y

---

## Pre-Delivery Checklist

Before delivering any UI code, verify:

- [ ] No emojis used as icons (use SVG instead)
- [ ] All icons from consistent icon set (Heroicons/Lucide)
- [ ] `cursor-pointer` on all clickable elements
- [ ] Hover states with smooth transitions (150-300ms)
- [ ] Light mode: text contrast 4.5:1 minimum
- [ ] Focus states visible for keyboard navigation
- [ ] `prefers-reduced-motion` respected
- [ ] Responsive: 375px, 768px, 1024px, 1440px
- [ ] No content hidden behind fixed navbars
- [ ] No horizontal scroll on mobile
