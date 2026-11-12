"""Builds the animated pictures of the README into docs/readme/:
hero.svg, flow.svg, net.svg, mem.svg, keys.svg and the still mark.svg.

    python docs/readme/src/build-svg.py

Text is drawn as outlines of Segoe UI Variable (Windows 11): the picture looks the same on any machine and needs no
font. Motion is CSS inside the SVG (GitHub shows an SVG as <img>: no script, CSS animation works); with
prefers-reduced-motion the pictures stand still in their final state.
The mark — three voice bars fading into the text caret, "Verba volant, scripta manent" — uses the numbers of
src/icons.rs (grid 120×120: caret 8×88, bars 14 wide, 72 → 50 → 28 tall).
"""
import os
import sys

[s.reconfigure(encoding="utf-8", errors="backslashreplace") for s in (sys.stdout, sys.stderr) if hasattr(s, "reconfigure")]

sys.path.insert(0, os.path.dirname(__file__))
from textpath import TextPath  # noqa: E402

FONT = r'C:\Windows\Fonts\SegUIVar.ttf'
MONO = r'C:\Windows\Fonts\CascadiaMono.ttf'
OUT = os.path.normpath(os.path.join(os.path.dirname(__file__), '..'))

BG0, BG1, BG2, BG3 = '#161513', '#1b1917', '#22201e', '#2c2a26'
FG0, FG1, FG2 = '#ebe9e6', '#aba69f', '#85817a'
LINE = '#3a3733'
CORAL, AMBER, OK, INK = '#E5484D', '#F5A524', '#69bb94', '#1b1917'
EASE = 'cubic-bezier(.2,.7,.2,1)'
SPRING = 'cubic-bezier(.34,1.56,.64,1)'
IO = 'cubic-bezier(.65,0,.35,1)'

bold = TextPath(FONT, {'wght': 700, 'opsz': 36})
semi = TextPath(FONT, {'wght': 600, 'opsz': 36})
semi_t = TextPath(FONT, {'wght': 600, 'opsz': 10.5})
reg = TextPath(FONT, {'wght': 400, 'opsz': 36})
reg_t = TextPath(FONT, {'wght': 400, 'opsz': 10.5})
mono = TextPath(MONO, {'wght': 400})

# the mark, grid 120×120 (src/icons.rs): bars (x, height), caret
BARS = [(21.5, 72), (43.5, 50), (65.5, 28)]
BAR_W, CARET_X, CARET_W, CARET_H = 14, 90.5, 8, 88
PLATE_RX, GLYPH_SCALE = 27, 0.78


def mark_shapes(bar_fill, caret_fill, cls=''):
    """the bars and the caret in the 120-grid; each bar is <rect class="{cls}b{i}">, the caret {cls}c"""
    out = ''
    for i, (x, h) in enumerate(BARS):
        out += f'<rect class="{cls}b{i}" x="{x}" y="{60 - h / 2}" width="{BAR_W}" height="{h}" rx="{BAR_W / 2}" fill="{bar_fill}"/>'
    out += f'<rect class="{cls}c" x="{CARET_X}" y="{60 - CARET_H / 2}" width="{CARET_W}" height="{CARET_H}" rx="{CARET_W / 2}" fill="{caret_fill}"/>'
    return out


def plate(x, y, size, cls=''):
    """the app icon: coral plate, white voice, dark caret — at (x, y), side `size`"""
    k = size / 120
    return (f'<g transform="translate({x:.2f} {y:.2f}) scale({k:.4f})"><g class="{cls}plate">'
            f'<rect width="120" height="120" rx="{PLATE_RX}" fill="{CORAL}"/>'
            f'<g transform="translate(60 60) scale({GLYPH_SCALE}) translate(-60 -60)">{mark_shapes("#ffffff", INK, cls)}</g>'
            f'</g></g>')


def panel(W, H):
    return f'<rect x="1" y="1" width="{W - 2}" height="{H - 2}" rx="28" fill="{BG0}" stroke="#ffffff" stroke-opacity=".07" stroke-width="2"/>'


def svg(name, W, H, title, css, body, bg=True):
    out = f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" aria-label="{title}">
<title>{title}</title>
<style>
  .fb {{ transform-box: fill-box; transform-origin: center; }}
{css}</style>
{panel(W, H) if bg else ''}
{body}
</svg>
'''
    path = os.path.join(OUT, name)
    with open(path, 'w', encoding='utf-8', newline='\n') as f:
        f.write(out)
    print(f'{path}: {os.path.getsize(path) // 1024} КБ')


def text(tp, s, size, x, y, fill, anchor='start', cls='', tracking=0.0):
    d, w = tp.path(s, size, x, y, tracking=tracking, anchor=anchor)
    c = f' class="{cls}"' if cls else ''
    return f'<path{c} fill="{fill}" d="{d}"/>', w


def reduced(extra=''):
    return f'  @media (prefers-reduced-motion: reduce) {{ * {{ animation: none !important; }} {extra} }}\n'


# ---------------------------------------------------------------------------------------------------------------------
# hero.svg — the plate pops, the voice rises, the word types itself; then forever: the voice speaks, flies into the
# caret (verba volant) and the caret stays blinking (scripta manent), the voice comes back
def hero():
    W, H = 1280, 420
    P = 128  # plate side
    word = 'Verba'
    ws = 118
    word_w = bold.width(word, ws)
    gap = 34
    x0 = W / 2 - (P + gap + word_w) / 2
    py = 70
    cy = py + P / 2
    letters = ''
    cx = x0 + P + gap
    for i, ch in enumerate(word):
        d, w = bold.path(ch, ws, cx, cy + 42)
        letters += f'<path class="l l{i}" fill="{FG0}" d="{d}"/>'
        cx += w
    a = semi.width('Держишь хоткей, говоришь', 32)
    b = reg.width(' — текст встаёт в поле', 32)
    tx = W / 2 - (a + b) / 2
    tag1, _ = text(semi, 'Держишь хоткей, говоришь', 32, tx, 296, FG0)
    tag2, _ = text(reg, ' — текст встаёт в поле', 32, tx + a, 296, FG1)
    motto, _ = text(reg_t, 'VERBA VOLANT, SCRIPTA MANENT', 16, W / 2, 356, FG2, anchor='middle', tracking=0.22)

    # one loop of the voice, 9 s: rise → speak → hold → fly into the caret → gone
    L = 9.0
    css = ''
    k = P / 120 * GLYPH_SCALE  # grid → hero px
    for i, (x, h) in enumerate(BARS):
        fly = (CARET_X - x) * 0.92  # to the caret, in grid units (the transform is inside the glyph group)
        w1, w2, w3 = [1.22, 1.3, 1.16][i], [0.7, 0.62, 0.78][i], [1.12, 1.18, 1.08][i]
        css += f'''  .hb{i} {{ animation: hv{i} {L}s {IO} {0.55 + i * 0.09:.2f}s infinite both; }}
  @keyframes hv{i} {{
    0% {{ transform: translateX(0) scaleY(0); opacity: 0; }}
    2% {{ opacity: 1; }}
    8% {{ transform: translateX(0) scaleY(1.08); animation-timing-function: ease-out; }}
    11% {{ transform: translateX(0) scaleY(1); }}
    16% {{ transform: translateX(0) scaleY({w1}); }}
    20% {{ transform: translateX(0) scaleY({w2}); }}
    24% {{ transform: translateX(0) scaleY({w3}); }}
    28%, 66% {{ transform: translateX(0) scaleY(1); opacity: 1; }}
    76% {{ transform: translateX({fly:.1f}px) scaleY(.15); opacity: 0; }}
    100% {{ transform: translateX({fly:.1f}px) scaleY(0); opacity: 0; }}
  }}
'''
    css += f'''  .hb0, .hb1, .hb2 {{ transform-box: fill-box; transform-origin: center; }}
  .hplate {{ transform-box: fill-box; transform-origin: center; animation: pop .55s {SPRING} .05s both; }}
  /* the caret never blinks out: the plate is never empty; it swells when the voice flies into it */
  .hc {{ transform-box: fill-box; transform-origin: center; animation: fade .45s ease .3s both, absorb {L}s ease-out .64s infinite both; }}
  @keyframes absorb {{ 0%, 71% {{ transform: scaleY(1); }} 75% {{ transform: scaleY(1.12); }} 82%, 100% {{ transform: scaleY(1); }} }}
  .l {{ opacity: 0; animation: type .01s linear both; }}
{''.join(f"  .l{i} {{ animation-delay: {1.0 + i * 0.09:.2f}s; }}{chr(10)}" for i in range(len(word)))}  .tag {{ animation: rise .7s {EASE} 1.55s both; }}
  .motto {{ animation: fade 1.2s ease 2.1s both; }}
  @keyframes pop {{ from {{ opacity: 0; transform: scale(.82); }} to {{ opacity: 1; transform: none; }} }}
  @keyframes type {{ to {{ opacity: 1; }} }}
  @keyframes rise {{ from {{ opacity: 0; transform: translateY(12px); }} to {{ opacity: 1; transform: none; }} }}
  @keyframes fade {{ from {{ opacity: 0; }} to {{ opacity: 1; }} }}
'''
    css += reduced('.l { opacity: 1; }')
    body = f'''{plate(x0, py, P, 'h')}
{letters}
<g class="tag">{tag1}{tag2}</g>
<g class="motto">{motto}</g>'''
    svg('hero.svg', W, H, 'Verba — держишь хоткей, говоришь, текст встаёт в поле', css, body)


# ---------------------------------------------------------------------------------------------------------------------
# flow.svg — four steps; the light runs along them; under the last three — «≈ 1 с»
ICONS = {
    'key': '<rect x="2.5" y="6.5" width="19" height="11" rx="3"/><path d="M7.5 13.5h9"/>',
    'wave': '<path class="w0 fb" d="M5 9v6"/><path class="w1 fb" d="M9.5 5v14"/><path class="w2 fb" d="M14 8v8"/><path class="w3 fb" d="M18.5 10.5v3"/>',
    'cloud': '<path d="M7.2 18.5h9.6a4.3 4.3 0 0 0 .5-8.57A5.8 5.8 0 0 0 6.2 9.4a4.6 4.6 0 0 0 1 9.1z"/>',
    'field': '<rect x="2.5" y="5.5" width="19" height="13" rx="3"/><path d="M6.5 12h6.5"/><path class="fc" d="M16 9.5v5"/>',
}


def flow():
    W, H = 1280, 470
    n, bw, bh, gap = 4, 262, 186, 40
    x0 = (W - (n * bw + (n - 1) * gap)) / 2
    y0 = 44
    steps = [
        ('key', 'Ctrl + Space', 'держишь и говоришь', CORAL),
        ('wave', 'Запись', '16 кГц, без тишины', CORAL),
        ('cloud', 'Распознавание', 'OpenRouter или ChatGPT', AMBER),
        ('field', 'Текст в поле', 'где был курсор', FG0),
    ]
    # the cycle, 8 s: talking is long, the rest is quick
    L = 8.0
    win = [(0, 40), (40, 56), (56, 74), (74, 94)]
    body, css = '', ''
    for i, (icon, title, line, col) in enumerate(steps):
        x = x0 + i * (bw + gap)
        nd, _ = text(mono, str(i + 1), 18, x + bw - 26, y0 + 42, FG2, anchor='end')
        td, _ = text(semi, title, 28, x + 28, y0 + 120, FG0)
        ld, _ = text(reg_t, line, 20, x + 28, y0 + 154, FG1)
        body += f'''<g>
  <rect x="{x}" y="{y0}" width="{bw}" height="{bh}" rx="20" fill="{BG2}"/>
  <rect class="hl hl{i}" x="{x}" y="{y0}" width="{bw}" height="{bh}" rx="20" fill="{col}" fill-opacity=".09"/>
  <g class="ic{i}" transform="translate({x + 26} {y0 + 26}) scale(1.4)" fill="none" stroke="{col}" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">{ICONS[icon]}</g>
  {nd}{td}{ld}
</g>
'''
        a, b = win[i]
        css += (f'  .hl{i} {{ opacity: 0; animation: hl{i} {L}s linear infinite both; }}\n'
                f'  @keyframes hl{i} {{ 0%, {a}% {{ opacity: 0; }} {a + 2}%, {b - 2}% {{ opacity: 1; }} {b + 1}%, 100% {{ opacity: 0; }} }}\n')
        if i < n - 1:
            ax = x + bw + 8
            body += f'<path d="M{ax},{y0 + bh / 2}h{gap - 16}" stroke="{LINE}" stroke-width="2" stroke-linecap="round"/>'
            body += f'<path d="M{ax + gap - 22},{y0 + bh / 2 - 5}l6 5-6 5" fill="none" stroke="{LINE}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>'
    # micro motion inside the cards, in step with the light
    css += f'''  .ic0 {{ animation: press {L}s ease infinite; }}
  @keyframes press {{ 0%, 3% {{ transform: translate({x0 + 26}px, {y0 + 26}px) scale(1.4); }} 5%, 38% {{ transform: translate({x0 + 26}px, {y0 + 28}px) scale(1.4); }} 41%, 100% {{ transform: translate({x0 + 26}px, {y0 + 26}px) scale(1.4); }} }}
'''
    for j in range(4):
        css += f'  .w{j} {{ animation: talk{j % 2} .{5 + j}s ease-in-out {j * 0.07:.2f}s infinite alternate; }}\n'
    css += '''  @keyframes talk0 { from { transform: scaleY(1); } to { transform: scaleY(.45); } }
  @keyframes talk1 { from { transform: scaleY(.5); } to { transform: scaleY(1.15); } }
  .fc { animation: blink 1.06s steps(1) infinite; }
  @keyframes blink { 0% { opacity: 1; } 50% { opacity: 0; } }
'''
    # «отпустил → текст ≈ 1 с» under steps 2–4
    bx1 = x0 + (bw + gap) + 20
    bx2 = x0 + 3 * (bw + gap) + bw - 20
    by = y0 + bh + 62
    body += f'''<path d="M{bx1},{by - 14}v14H{bx2}v-14" fill="none" stroke="{LINE}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>
<path class="bar" d="M{bx1},{by}H{bx2}" stroke="{CORAL}" stroke-width="3" stroke-linecap="round" pathLength="1" stroke-dasharray="1 1"/>
'''
    lab, lw = text(semi, '≈ 1 с', 30, (bx1 + bx2) / 2, by + 58, FG0, anchor='middle')
    sub, _ = text(reg_t, 'от отпускания хоткея до текста', 20, (bx1 + bx2) / 2, by + 90, FG1, anchor='middle')
    body += lab + sub
    s0, s1 = win[1][0], win[3][1]
    css += f'''  .bar {{ stroke-dashoffset: 1; animation: barf {L}s linear infinite both; }}
  @keyframes barf {{ 0%, {s0}% {{ stroke-dashoffset: 1; opacity: 1; }} {s1}% {{ stroke-dashoffset: 0; opacity: 1; }} 100% {{ stroke-dashoffset: 0; opacity: 0; }} }}
'''
    # «держишь» under step 1
    hx1, hx2 = x0 + 20, x0 + bw - 20
    body += f'<path d="M{hx1},{by - 14}v14H{hx2}v-14" fill="none" stroke="{LINE}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>'
    body += f'<path class="hold" d="M{hx1},{by}H{hx2}" stroke="{FG2}" stroke-width="3" stroke-linecap="round" pathLength="1" stroke-dasharray="1 1"/>'
    hl, _ = text(reg_t, 'сколько говоришь', 20, (hx1 + hx2) / 2, by + 58, FG1, anchor='middle')
    body += hl
    css += f'''  .hold {{ stroke-dashoffset: 1; animation: holdf {L}s linear infinite both; }}
  @keyframes holdf {{ 0% {{ stroke-dashoffset: 1; opacity: 1; }} {win[0][1]}% {{ stroke-dashoffset: 0; opacity: 1; }} 100% {{ stroke-dashoffset: 0; opacity: 0; }} }}
'''
    css += reduced('.hl { opacity: 0; } .bar, .hold { stroke-dashoffset: 0; }')
    svg('flow.svg', W, H, 'Как работает Verba: держишь Ctrl+Space и говоришь → запись → распознавание → текст в поле, около 1 с', css, body)


# ---------------------------------------------------------------------------------------------------------------------
# net.svg — the first sending hangs, the same recording leaves on a new connection after 4 s and wins
def net():
    W, H = 1280, 470
    L = 9.0
    xa, xb = 250, 1030  # lanes
    ys = [138, 222, 306]
    body, css = '', ''
    # Verba
    body += plate(96, ys[1] - 52, 104, 'n')
    vl, _ = text(semi_t, 'Verba', 20, 148, ys[1] + 86, FG1, anchor='middle')
    body += vl
    # the service
    body += f'<circle cx="1132" cy="{ys[1]}" r="52" fill="{BG2}"/>'
    body += (f'<g transform="translate({1132 - 25} {ys[1] - 25}) scale(2.1)" fill="none" stroke="{FG1}" stroke-width="1.6" '
             f'stroke-linecap="round" stroke-linejoin="round">{ICONS["cloud"]}</g>')
    sl, _ = text(semi_t, 'сервис', 20, 1132, ys[1] + 86, FG1, anchor='middle')
    body += sl
    for i, y in enumerate(ys):
        body += f'<path class="lane{i}" d="M{xa},{y}H{xb}" stroke="{LINE}" stroke-width="2" stroke-linecap="round"/>'
        nd, _ = text(mono, f'{i + 1}', 16, xa - 22, y + 6, FG2, anchor='end')
        body += nd
    # packets: a coral chip (the recording) and a green chip (the text) coming back
    def chip(cls, y, col, label, lc):
        lw = semi_t.width(label, 15)
        w = lw + 22
        d, _ = semi_t.path(label, 15, xa - w / 2 + 11, y + 5.5)
        return f'<g class="{cls}"><rect x="{xa - w / 2}" y="{y - 13}" width="{w}" height="26" rx="13" fill="{col}"/><path fill="{lc}" d="{d}"/></g>'
    body += chip('p1', ys[0], CORAL, 'WAV', '#ffffff')
    body += chip('p2', ys[1], CORAL, 'WAV', '#ffffff')
    body += chip('r2', ys[1], OK, 'текст', INK)
    # the wait on lane 1: a bar fills to «4 с»
    wx1, wx2 = 596, 900
    body += f'<path d="M{wx1},{ys[0] - 32}H{wx2}" stroke="{BG3}" stroke-width="6" stroke-linecap="round"/>'
    body += f'<path class="wait" d="M{wx1},{ys[0] - 32}H{wx2}" stroke="{AMBER}" stroke-width="6" stroke-linecap="round" pathLength="1" stroke-dasharray="1 1"/>'
    w4, _ = text(semi_t, '4 с без ответа', 18, wx2 + 18, ys[0] - 26, AMBER, cls='w4')
    body += w4
    x1, _ = text(semi_t, 'новое соединение', 18, xa + 8, ys[1] - 26, FG1, cls='newc')
    body += x1
    ok, _ = text(semi_t, 'первый ответ — в поле', 18, xb - 8, ys[1] - 26, OK, anchor='end', cls='okl')
    body += ok
    cut, _ = text(semi_t, 'брошена', 18, 560, ys[0] + 34, FG2, cls='cut')
    body += cut
    spare, _ = text(reg_t, 'третья — если молчит и вторая', 18, xa + 8, ys[2] - 22, FG2)
    body += spare
    # the timeline of one loop (in %): 1 leaves 2, hangs at 10; wait 10→46; 2 leaves 46 → arrives 60; reply 62 → 74
    run = xb - xa
    hang = 560 - xa
    css += f'''  .p1 {{ animation: p1 {L}s {IO} infinite both; }}
  @keyframes p1 {{
    0% {{ transform: translateX(0); opacity: 0; }} 2% {{ transform: translateX(0); opacity: 1; }}
    11% {{ transform: translateX({hang}px); }} 76% {{ transform: translateX({hang}px); opacity: 1; }}
    82%, 100% {{ transform: translateX({hang}px); opacity: 0; }}
  }}
  .lane0 {{ animation: dim {L}s linear infinite both; }}
  @keyframes dim {{ 0%, 74% {{ stroke-opacity: 1; }} 80%, 96% {{ stroke-opacity: .35; }} 100% {{ stroke-opacity: 1; }} }}
  .wait {{ stroke-dashoffset: 1; animation: wait {L}s linear infinite both; }}
  @keyframes wait {{ 0%, 11% {{ stroke-dashoffset: 1; opacity: 1; }} 46% {{ stroke-dashoffset: 0; opacity: 1; }} 90% {{ stroke-dashoffset: 0; opacity: 1; }} 96%, 100% {{ stroke-dashoffset: 0; opacity: 0; }} }}
  .w4 {{ animation: show46 {L}s linear infinite both; }}
  @keyframes show46 {{ 0%, 45% {{ opacity: 0; }} 48%, 92% {{ opacity: 1; }} 96%, 100% {{ opacity: 0; }} }}
  .newc {{ animation: newc {L}s linear infinite both; }}
  @keyframes newc {{ 0%, 45% {{ opacity: 0; }} 48%, 62% {{ opacity: 1; }} 66%, 100% {{ opacity: 0; }} }}
  .p2 {{ animation: p2 {L}s {IO} infinite both; }}
  @keyframes p2 {{
    0%, 46% {{ transform: translateX(0); opacity: 0; }} 48% {{ transform: translateX(0); opacity: 1; }}
    60% {{ transform: translateX({run}px); opacity: 1; }} 62%, 100% {{ transform: translateX({run}px); opacity: 0; }}
  }}
  .r2 {{ animation: r2 {L}s {IO} infinite both; }}
  @keyframes r2 {{
    0%, 61% {{ transform: translateX({run}px); opacity: 0; }} 63% {{ transform: translateX({run}px); opacity: 1; }}
    74% {{ transform: translateX(0); opacity: 1; }} 77%, 100% {{ transform: translateX(0); opacity: 0; }}
  }}
  .lane1 {{ animation: lit {L}s linear infinite both; }}
  @keyframes lit {{ 0%, 46% {{ stroke: {LINE}; }} 50%, 74% {{ stroke: #5a564f; }} 80%, 100% {{ stroke: {LINE}; }} }}
  .okl {{ animation: okl {L}s linear infinite both; }}
  @keyframes okl {{ 0%, 73% {{ opacity: 0; }} 76%, 94% {{ opacity: 1; }} 98%, 100% {{ opacity: 0; }} }}
  .cut {{ animation: cut {L}s linear infinite both; }}
  @keyframes cut {{ 0%, 76% {{ opacity: 0; }} 80%, 94% {{ opacity: 1; }} 98%, 100% {{ opacity: 0; }} }}
  .nplate {{ transform-box: fill-box; transform-origin: center; animation: got {L}s ease-out infinite both; }}
  @keyframes got {{ 0%, 73% {{ transform: scale(1); }} 76% {{ transform: scale(1.1); }} 80%, 100% {{ transform: scale(1); }} }}
'''
    c1, _ = text(reg_t, 'Нет ответа за 4 с (+3 с на МБ) — та же запись уходит по новому соединению.', 21, W / 2, 398, FG1, anchor='middle')
    c2, _ = text(reg_t, 'До трёх отправок, побеждает первый ответ.', 21, W / 2, 430, FG2, anchor='middle')
    body += c1 + c2
    css += reduced('.p1 { transform: translateX(310px); } .p2, .r2 { opacity: 0; } .wait { stroke-dashoffset: 0; }')
    svg('net.svg', W, H, 'Зависшее соединение: через 4 с та же запись уходит по новому, побеждает первый ответ', css, body)


# ---------------------------------------------------------------------------------------------------------------------
# mem.svg — memory at rest: the bars grow, the processes pop
def mem(verba_mb, verba_procs, handy_mb, handy_procs):
    W, H = 1280, 330
    lx, bx, bmax = 70, 330, 700
    rows = [
        ('Handy', 'Tauri, WebView2', handy_mb, handy_procs, '#5a564f', FG1),
        ('Verba', 'Rust, Win32', verba_mb, verba_procs, CORAL, FG0),
    ]
    body, css = '', ''
    for i, (name, sub, mb, procs, col, tc) in enumerate(rows):
        y = 92 + i * 118
        nd, _ = text(semi, name, 32, lx, y + 12, tc)
        sd, _ = text(reg_t, sub, 19, lx, y + 42, FG2)
        w = max(14, bmax * mb / handy_mb)
        body += nd + sd
        body += f'<rect x="{bx}" y="{y - 18}" width="{bmax}" height="36" rx="10" fill="{BG2}"/>'
        body += f'<rect class="bar{i}" x="{bx}" y="{y - 18}" width="{w:.1f}" height="36" rx="10" fill="{col}"/>'
        md, _ = text(semi, f'{mb:g} МБ', 30, bx + bmax + 40, y + 11, tc, cls=f'num{i}')
        body += md
        for p in range(procs):
            body += f'<rect class="pr pr{i}_{p}" x="{bx + p * 22}" y="{y + 30}" width="14" height="14" rx="4" fill="{col}" fill-opacity=".85"/>'
            css += f'  .pr{i}_{p} {{ animation: pop .4s {SPRING} {0.9 + i * 0.5 + p * 0.07:.2f}s both; }}\n'
        pd, _ = text(reg_t, f'{procs} ' + ('процесс' if procs == 1 else 'процессов'), 18, bx + procs * 22 + 10, y + 43, FG2, cls=f'pc{i}')
        body += pd
        css += (f'  .bar{i} {{ transform-box: fill-box; transform-origin: left center; animation: grow 1.1s {EASE} {0.2 + i * 0.5:.1f}s both; }}\n'
                f'  .num{i}, .pc{i} {{ animation: fade .6s ease {0.8 + i * 0.5:.1f}s both; }}\n')
    cap, _ = text(reg_t, 'Память в простое (private bytes), замер на одном ПК 12.11.2026', 18, lx, H - 30, FG2)
    body += cap
    css += f'''  .pr {{ transform-box: fill-box; transform-origin: center; }}
  @keyframes grow {{ from {{ transform: scaleX(0); }} to {{ transform: none; }} }}
  @keyframes pop {{ from {{ opacity: 0; transform: scale(.3); }} to {{ opacity: 1; transform: none; }} }}
  @keyframes fade {{ from {{ opacity: 0; }} to {{ opacity: 1; }} }}
'''
    css += reduced()
    svg('mem.svg', W, H, f'Память в простое: Handy {handy_mb:g} МБ в {handy_procs} процессах, Verba {verba_mb:g} МБ в одном', css, body)


# ---------------------------------------------------------------------------------------------------------------------
# keys.svg — four shortcuts, pressed in turn
def keys():
    W, H = 1280, 360
    cards = [
        (['Ctrl', 'Space'], 'держать — говорить', CORAL),
        (['Esc'], 'отменить запись', FG1),
        (['Ctrl', 'Alt', 'P'], 'пауза: сочетание уходит в игру', AMBER),
        (['Ctrl', 'Alt', 'V'], 'вставить последнее', OK),
    ]
    cw, ch, gx, gy, m = 568, 132, 24, 24, 48
    body, css = '', ''
    L = 6.0
    for i, (ks, label, col) in enumerate(cards):
        x = m + (i % 2) * (cw + gx)
        y = m + (i // 2) * (ch + gy)
        body += f'<rect x="{x}" y="{y}" width="{cw}" height="{ch}" rx="20" fill="{BG2}"/>'
        kx = x + 28
        ky = y + 30
        for j, k in enumerate(ks):
            kw = max(54, semi_t.width(k, 20) + 32)
            d, _ = semi_t.path(k, 20, kx + kw / 2, ky + 34, anchor='middle')
            body += (f'<g class="k k{i}"><rect x="{kx}" y="{ky}" width="{kw:.1f}" height="50" rx="11" fill="{BG3}"/>'
                     f'<rect class="kf{i}" x="{kx}" y="{ky}" width="{kw:.1f}" height="50" rx="11" fill="{col}" fill-opacity=".22"/>'
                     f'<path fill="{FG0}" d="{d}"/></g>')
            kx += kw + 10
            if j < len(ks) - 1:
                pd, _ = text(reg_t, '+', 20, kx + 2, ky + 33, FG2)
                body += pd
                kx += 22
        ld, _ = text(reg_t, label, 22, x + 28, y + ch - 24, FG1)
        body += ld
        a = i * 22
        css += (f'  .k{i} {{ animation: kp{i} {L}s ease infinite both; }}\n'
                f'  @keyframes kp{i} {{ 0%, {a}% {{ transform: none; }} {a + 2}%, {a + 14}% {{ transform: translateY(3px); }} {a + 17}%, 100% {{ transform: none; }} }}\n'
                f'  .kf{i} {{ opacity: 0; animation: kf{i} {L}s ease infinite both; }}\n'
                f'  @keyframes kf{i} {{ 0%, {a}% {{ opacity: 0; }} {a + 2}%, {a + 14}% {{ opacity: 1; }} {a + 18}%, 100% {{ opacity: 0; }} }}\n')
    css += reduced('.kf0, .kf1, .kf2, .kf3 { opacity: 0; }')
    svg('keys.svg', W, H, 'Хоткеи: Ctrl+Space — диктовка, Esc — отмена, Ctrl+Alt+P — пауза, Ctrl+Alt+V — вставить последнее', css, body, bg=False)


def static_mark():
    """mark.svg — the still app icon for the footer and the page"""
    out = f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 120 120" width="120" height="120" role="img" aria-label="Verba">
<title>Verba</title>
{plate(0, 0, 120)}
</svg>
'''
    path = os.path.join(OUT, 'mark.svg')
    with open(path, 'w', encoding='utf-8', newline='\n') as f:
        f.write(out)
    print(f'{path}: {os.path.getsize(path)} Б')


if __name__ == '__main__':
    hero()
    flow()
    net()
    # private bytes at rest, one PC, 12.11.2026: Verba release build after 20 s — 2,1 МБ, CPU 0,016 с за 10 с;
    # Handy 0.9.8 — handy.exe + 7 × msedgewebview2.exe summed
    mem(verba_mb=2, verba_procs=1, handy_mb=286, handy_procs=8)
    keys()
    static_mark()
