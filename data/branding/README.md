# Eloquawk artwork

- `eloquawk-app-icon.svg`: the compact penguin holding a sealed envelope, used for the app
  header, the sign-in screen and the installed Linux desktop icon. It shares the penguin with
  antarctical's icon so the korora apps look like a family.
- `eloquawk-app-icon.png`: 512 px export of the SVG, installed as the 512x512 hicolor icon.

The SVG has a transparent background and editable, named groups. It contains no embedded
bitmap images, fonts, scripts or external resources. GPUI embeds it in the binary, so the app
needs no asset directory at run time. The Nix package installs both files under
`share/icons/hicolor` with the app id `korora.tech.Eloquawk` as the icon name, which is also
the name notifications ask for.

After changing the SVG, regenerate the PNG with librsvg:

```sh
nix shell nixpkgs#librsvg -c rsvg-convert \
  --width 512 --height 512 \
  --output data/branding/eloquawk-app-icon.png \
  data/branding/eloquawk-app-icon.svg
```
