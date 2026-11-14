# Android resource overlay (spec 025)

dx 0.7 always writes its default launcher icon (the green Android robot)
and an `app_name` of the bin name in Pascal case, with no setting to change
either. `scripts/android-apk.sh` copies this tree over the generated
`res/` after `dx build`, points the manifest's `android:banner` at
`pharos_banner`, and runs gradle again.

| File | What |
|---|---|
| `drawable-v24/ic_launcher_foreground.xml` | Adaptive icon: the Beam P mark |
| `drawable/ic_launcher_background.xml` | Adaptive icon: Pharos Night tile colour |
| `drawable/ic_launcher_monochrome.xml` | Adaptive icon: one-colour mark for Android 13 themed icons |
| `mipmap-anydpi-v26/ic_launcher.xml` | Adaptive icon: background, foreground and monochrome layers (dx's has no monochrome) |
| `drawable/ic_stat_pharos.xml` | Status bar and playback notification small icon: the P mark, white on transparent |
| `mipmap-*/ic_launcher.webp` | Bitmap icon, a full dark circle: Android Auto's launcher draws it inside a white disc, so the rounded square left a white ring |
| `drawable-xhdpi/pharos_banner.png` | 320x180 TV launcher banner: `assets/brand/logo.svg` on `#0e1116` |
| `values/strings.xml` | `app_name` = Pharos |
| `values/styles.xml` | `AppTheme`: dark, with a Pharos Night window background, so start-up shows no white |

The rasters are regenerated from the brand assets with ImageMagick:

    magick -background none -density 400 ../../assets/brand/logo.svg -resize 248x logo.png
    magick -size 320x180 xc:'#0e1116' logo.png -gravity center -composite -depth 8 -strip drawable-xhdpi/pharos_banner.png
    magick ../../assets/brand/icon-512.png -resize 96x96 \
      \( -size 96x96 xc:'#0E1116' \) +swap -gravity center -compose over -composite \
      \( -size 96x96 xc:black -fill white -draw 'circle 48,48 48,0' \) -alpha off -compose copyopacity -composite \
      -define webp:lossless=true webp:mipmap-xhdpi/ic_launcher.webp   # 48/72/96/144/192 for m/h/xh/xxh/xxxhdpi
