# Marks

| file | what it is for |
|---|---|
| `blinker-mark.svg` | the square mark — favicon, avatar, anywhere small |
| `blinker-banner.svg` | the lockup — the README header |
| `blinker-social.png` | 1280×640, for GitHub's social preview, which will not take SVG |

The mark is three chain links with the middle one being re-forged: two cold, one
hot. That is the product in one picture — a link that has already been made is
not made again.

`blinker-social.png` is generated from `blinker-banner.svg`, so edit the SVG and
regenerate rather than editing the PNG.

The banner pins its text with `textLength`. It renders in whatever monospace the
viewer happens to have, and without pinning the wordmark overflows the frame on
any machine whose default is wider than the one it was drawn on.
