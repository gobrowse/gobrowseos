# starter-ui — minimal THEME package

Copy this directory to start a new theme:

```bash
cp -r examples/starter-ui my-theme
cd my-theme
# edit manifest.json (name, version, theme.variables) and theme.css
../tools/ui-package validate .
../tools/ui-package preview .   # needs running server
../tools/ui-package install . --approve
```

`manifest.json` is the source of truth. `theme.css` is optional — the server generates CSS from `manifest.theme.variables` if this file is absent.

See `docs/UI-SDK.md` for the full contract and FULL_UI tutorial.
