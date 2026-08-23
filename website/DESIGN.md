# ZOrca Website Design

This document describes the public website's visual system and the conventions
contributors should preserve when changing `website/`.

## Design direction

The site presents ZOrca through the product itself: a concise introduction, real
application captures, current capabilities, an honest comparison, future
direction, and installation instructions.

The visual identity combines a cobalt, violet, and coral mark with a dark native
application interface and an illustrated high-desert landscape. Product sections
use restrained editorial layouts so screenshots and technical copy remain easy
to read.

## Assets

- `zorca-logo-m1.png`: full-colour marketing logo
- `zorca-logo-m1-128.png`: compact website logo and favicon
- `zorca-hero.webp`: desktop hero illustration
- `zorca-hero-mobile.webp`: mobile hero illustration
- `zorca-workspace.webp`: full-size application overview
- `zorca-workspace-800.webp`: responsive application overview
- `zorca-projects.mp4`: multi-project workflow capture
- `zorca-worktree.mp4`: isolated worktree workflow capture
- `zorca-git-review.mp4`: Git review workflow capture
- `fonts/archivo-latin.woff2`: self-hosted Archivo webfont

Keep the white fused `ZO` geometry unchanged. Colour belongs to the background
tile, not the symbol.

Product captures must show the current application using demonstration
repositories without personal, confidential, or customer information. Update a
capture when the visible interface or documented workflow materially changes.

## Colour

| Role | Value | Use |
| --- | --- | --- |
| Midnight | `#071833` | Hero and primary dark surfaces |
| Deep navy | `#0D2852` | Supporting dark surfaces |
| Ink | `#11151B` | Primary text |
| Muted ink | `#59616D` | Supporting text |
| Paper | `#F7F7F4` | Main content background |
| Blue | `#304CFF` | Primary actions and active states |
| Violet | `#713CFF` | Brand transitions and calls to action |
| Coral | `#FF654B` | Warm brand accents |
| Planned field | `#E7EFF9` | Roadmap and comparison emphasis |

Use neutral surfaces for most interface elements. Reserve saturated brand
colours for identity, actions, and meaningful emphasis.

## Typography

Archivo is the prose typeface. Headings use weight 700, compact line-height, and
tracking no tighter than `-0.04em`. Body text uses weight 400 with a readable
line length near 70 characters. Use the platform monospace only for commands,
status labels, code, and table metadata.

## Layout

- Global content width: `min(1320px, 100% - 3rem)`
- Hero: navigation, centred product statement, one primary action, and a real
  application capture
- Current capabilities: alternating two-column product stories
- Zed foundation: dark capability inventory
- Comparison: factual table following current capabilities
- Roadmap: clearly labelled planned work without dates or progress claims
- Install: commands and current release-status information

At `850px`, two-column sections become single-column, roadmap rows stack, and
the capability grid becomes one column. At `560px`, spacing tightens, secondary
navigation is hidden, media uses mobile crops, comparison rows stack, and the
footer wraps. The page must not scroll horizontally.

## Components

### Navigation

Place the full-colour mark and wordmark on the left and product anchors with the
source link on the right. On narrow screens, keep the source icon and hide
secondary section links.

### Primary action

Use a blue-to-violet background with white text, a modest corner radius, and a
clear focus state. The hero has one primary action.

### Product media

Use rounded dark captures with restrained shadows. Identify each view with
descriptive alternative text or a caption. Load the hero image eagerly and
feature media lazily.

### Roadmap entries

Label every future item as planned. Do not show dates, progress percentages, or
imagery that could be mistaken for a released feature.

## Accessibility and motion

- Use semantic headings, landmarks, links, tables, figures, and captions.
- Keep all controls keyboard reachable with visible focus indicators.
- Provide descriptive alternative text for meaningful images.
- Maintain WCAG AA text contrast.
- Respect `prefers-reduced-motion`.
- Do not rely on colour alone to communicate meaning.
- Preserve a minimum 44-pixel target size for primary touch controls.

## Content rules

- Describe released behavior separately from planned work.
- Support current capabilities with authentic product captures.
- Credit Zed for inherited editor capabilities.
- Keep pre-alpha and distribution limitations visible and current.
- Do not publish fabricated users, testimonials, metrics, dates, prices, or
  benchmarks.
- Keep the website consistent with `README.md` and `PRODUCT.md`.
