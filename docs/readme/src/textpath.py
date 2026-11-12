"""Text → SVG path outlines from a local font (no font is needed where the SVG is shown).

    from textpath import TextPath
    tp = TextPath(r'C:\\Windows\\Fonts\\SegUIVar.ttf', {'wght': 600, 'opsz': 36})
    d, width = tp.path('Задачи', size=28, x=0, y=40, tracking=0)

Glyph advances come from hmtx and the kerning pairs of the font's GPOS 'kern' feature are not applied: at banner sizes
the difference is under 1 px. Variable fonts are instanced once per axis set.
"""
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont


class TextPath:
    def __init__(self, font_file, axes=None):
        font = TTFont(font_file)
        if axes and 'fvar' in font:
            # layout tables are not used here, and the instancer fails on the GPOS of Segoe UI Variable
            for tag in ('GPOS', 'GSUB', 'GDEF'):
                if tag in font:
                    del font[tag]
            font = instantiateVariableFont(font, axes)
        self.font = font
        self.cmap = font.getBestCmap()
        self.glyphs = font.getGlyphSet()
        self.upm = font['head'].unitsPerEm
        self.hmtx = font['hmtx']

    def width(self, text, size, tracking=0.0):
        scale = size / self.upm
        total = 0.0
        for ch in text:
            name = self.cmap.get(ord(ch))
            if name is None:
                name = self.cmap.get(ord('?'))
            total += self.hmtx[name][0] * scale + tracking * size
        return total - (tracking * size if text else 0)

    def path(self, text, size, x=0.0, y=0.0, tracking=0.0, anchor='start'):
        """SVG path data for `text` with its baseline at y; anchor start | middle | end."""
        scale = size / self.upm
        w = self.width(text, size, tracking)
        if anchor == 'middle':
            x -= w / 2
        elif anchor == 'end':
            x -= w
        pen = SVGPathPen(self.glyphs, ntos=lambda v: f'{v:.2f}'.rstrip('0').rstrip('.'))
        cx = x
        for ch in text:
            name = self.cmap.get(ord(ch)) or self.cmap.get(ord('?'))
            glyph = self.glyphs[name]
            glyph.draw(TransformPen(pen, (scale, 0, 0, -scale, cx, y)))
            cx += self.hmtx[name][0] * scale + tracking * size
        return pen.getCommands(), w
