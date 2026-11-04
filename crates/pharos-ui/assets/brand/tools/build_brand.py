"""Regenerates the brand SVGs and manifest.

usage: python3 build_brand.py <assets/brand dir> <InterDisplay-SemiBold.otf>
Needs fontTools. Then run raster.js for the PNG/ICO files.
"""
import sys,os,json
sys.path.insert(0,os.path.dirname(os.path.abspath(__file__)))
from outline import outline
OUT=sys.argv[1]; FONT=sys.argv[2]; os.makedirs(OUT,exist_ok=True)
NIGHT=dict(a="#4cb4ff",l="#ffd27a",t="#e6e8eb",bg="#0e1116")
DAY=dict(a="#0b63c5",l="#f0a020",t="#15181d",bg="#f7f8fa")
BEAM="M44 22 64 12V36L44 30Z"
BODY="M12 10a4 4 0 0 1 4-4H32a20 20 0 0 1 0 40H24V56a4 4 0 0 1-4 4H16a4 4 0 0 1-4-4Z M26 16.5V35.5L42 26Z"
PLAY="M26 16.5V35.5L42 26Z"
def mark_body(a,l,gid="beam",tx=""):
    return (f'<g{tx}><linearGradient id="{gid}" x1="44" x2="64" y1="0" y2="0" gradientUnits="userSpaceOnUse">'
            f'<stop offset="0" stop-color="{l}" stop-opacity=".9"/><stop offset="1" stop-color="{l}" stop-opacity="0"/></linearGradient>'
            f'<path d="{BEAM}" fill="url(#{gid})"/><path fill-rule="evenodd" fill="{a}" d="{BODY}"/><path d="{PLAY}" fill="{l}"/></g>')
def w(name,s): open(os.path.join(OUT,name),'w').write(s.strip()+"\n")
HDR='<!-- Pharos brand mark "Beam P". Original artwork; see README.md. -->\n'
# mark.svg: Pharos Night colours
w("mark.svg",HDR+f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" role="img" aria-label="Pharos">{mark_body(NIGHT["a"],NIGHT["l"])}</svg>')
# favicon.svg: follows the browser/OS colour scheme
w("favicon.svg",HDR+f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
<style>.a{{fill:{DAY["a"]}}}.l{{fill:{DAY["l"]}}}.s{{stop-color:{DAY["l"]}}}
@media (prefers-color-scheme:dark){{.a{{fill:{NIGHT["a"]}}}.l{{fill:{NIGHT["l"]}}}.s{{stop-color:{NIGHT["l"]}}}}}</style>
<linearGradient id="beam" x1="44" x2="64" y1="0" y2="0" gradientUnits="userSpaceOnUse"><stop class="s" offset="0" stop-opacity=".9"/><stop class="s" offset="1" stop-opacity="0"/></linearGradient>
<path d="{BEAM}" fill="url(#beam)"/><path class="a" fill-rule="evenodd" d="{BODY}"/><path class="l" d="{PLAY}"/></svg>''')
# wordmark
d,wd,upm,*_=outline("pharos",FONT,-0.015)
top=-1500; h=1950  # covers ascender (h) to descender (p)
w("wordmark.svg",HDR+f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 {top} {wd:.0f} {h}" role="img" aria-label="pharos"><path fill="currentColor" d="{d}"/></svg>')
# lockups: mark 64 tall; wordmark scaled to height 64*h/... place x-height centred
def lockup(c,name):
    s=46/h*1.0  # wordmark box height 46 in a 64 tall lockup
    s=44/1500*1.0
    sc=40/1056  # x-height 1056 -> 40 units? keep simple: cap height ~ mark
    sc=0.0235
    wx=74; base=44
    W=wx+wd*sc+2
    return HDR+(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W:.1f} 64" role="img" aria-label="Pharos">'
       f'{mark_body(c["a"],c["l"])}<path fill="{c["t"]}" transform="translate({wx} {base}) scale({sc})" d="{d}"/></svg>')
w("logo.svg",lockup(NIGHT,"dark")); w("logo-light.svg",lockup(DAY,"light"))
# tile for raster icons
def tile(size_pad, rounded, bg=NIGHT["bg"]):
    # mark occupies (1-2*pad) of 64 canvas
    k=1-2*size_pad
    r=f' rx="{14 if rounded else 0}"'
    # Built apart: a backslash inside an f-string field needs Python 3.12.
    tx=f' transform="translate({32-38*k:.2f} {32-33*k:.2f}) scale({k:.4f})"'
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64"{r} fill="{bg}"/>'
            f'{mark_body(NIGHT["a"],NIGHT["l"],tx=tx)}</svg>')
w("_tile_any.svg",tile(0.14,True)); w("_tile_full.svg",tile(0.14,False)); w("_tile_mask.svg",tile(0.22,False))
w("manifest.webmanifest",json.dumps({
 "name":"Pharos","short_name":"Pharos","description":"Your media, served by Pharos.",
 "id":"/ui/","start_url":"/ui/","scope":"/ui/","display":"standalone",
 "background_color":NIGHT["bg"],"theme_color":NIGHT["bg"],
 "icons":[
  {"src":"/ui/assets/brand/icon-192.png","sizes":"192x192","type":"image/png","purpose":"any"},
  {"src":"/ui/assets/brand/icon-512.png","sizes":"512x512","type":"image/png","purpose":"any"},
  {"src":"/ui/assets/brand/icon-maskable-512.png","sizes":"512x512","type":"image/png","purpose":"maskable"},
  {"src":"/ui/assets/brand/favicon.svg","sizes":"any","type":"image/svg+xml","purpose":"any"}]},indent=2))
