import sys
from fontTools.ttLib import TTFont
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
def outline(text, font, track=0.0):
    f=TTFont(font); gs=f.getGlyphSet(); cmap=f.getBestCmap(); upm=f['head'].unitsPerEm
    pen=SVGPathPen(gs); x=0
    for ch in text:
        g=cmap[ord(ch)]; tp=TransformPen(pen,(1,0,0,-1,x,0)); gs[g].draw(tp)
        x+=gs[g].width+track*upm
    x-=track*upm
    asc=f['OS/2'].sCapHeight; xh=f['OS/2'].sxHeight; desc=-f['OS/2'].sTypoDescender
    return pen.getCommands(), x, upm, xh, f['hhea'].ascent, f['hhea'].descent
if __name__=='__main__':
    d,w,upm,xh,a,de=outline(sys.argv[1],sys.argv[2],float(sys.argv[3]))
    print(w,upm,xh,a,de); print(d[:200])
