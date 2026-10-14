"""Exercise the production console script over real browser SCTP channels.

The HTTP signalling boundary and screenshot source are controlled fixtures;
WebRTC, message types, artifact bytes, PNG rendering and gzip decoding are real.
Requires the local Python Playwright package and Chromium browser.
"""
import base64
import gzip
from pathlib import Path
import struct
import unittest
import zlib

from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[1]
TEMPLATE = ROOT / 'src/addons/cc/templates/contact_center/spa.html'
CHUNK = 16 * 1024


def png_fixture():
    def chunk(kind, data):
        return struct.pack('!I', len(data)) + kind + data + struct.pack('!I', zlib.crc32(kind + data))
    png = b'\x89PNG\r\n\x1a\n'
    png += chunk(b'IHDR', struct.pack('!IIBBBBB', 1, 1, 8, 2, 0, 0, 0))
    png += chunk(b'IDAT', zlib.compress(b'\x00\x00\xff\x00'))
    png += chunk(b'IEND', b'')
    # Valid PNG with a trailing chunk that resembles a control frame.
    return png.ljust(CHUNK, b'\x00') + b'{"type":"done"}'


SETUP = r'''
const AMI_BASE = '/pbx-test/ami/v1';
const ICE_SERVERS_PATH = '/pbx-test/iceservers';
const t = (_, fallback) => fallback;
let artifact = null, peer = null, selected = null;
const createObjectURL = URL.createObjectURL.bind(URL);
URL.createObjectURL = blob => { artifact = blob; return createObjectURL(blob); };
const payload = Uint8Array.from(atob(window.fixture.payload), c => c.charCodeAt(0));
const gathered = pc => new Promise((resolve, reject) => {
  if (pc.iceGatheringState === 'complete') return resolve();
  const timer = setTimeout(() => reject(new Error('answer gathering timeout')), 5000);
  pc.addEventListener('icegatheringstatechange', () => {
    if (pc.iceGatheringState === 'complete') { clearTimeout(timer); resolve(); }
  });
});
window.fetch = async (url, options) => {
  if (url === '/pbx-test/iceservers') return new Response('[]');
  if (url !== '/pbx-test/ami/v1/cc/admin_cmd') throw new Error('incorrect AMI path');
  const command = JSON.parse(options.body);
  if (command.cmd !== window.fixture.cmd || command.agent !== 'interop-agent') throw new Error('incorrect command');
  if (command.cmd === 'shot' && command.target !== window.fixture.target) throw new Error('incorrect screenshot target');
  peer = new RTCPeerConnection();
  peer.ondatachannel = event => {
    const dc = event.channel;
    dc.onopen = async () => {
      const stats = await peer.getStats();
      selected = [...stats.values()].find(s => s.type === 'candidate-pair' && s.state === 'succeeded');
      dc.send(JSON.stringify({type:'meta', cmd:command.cmd, mime:command.cmd === 'shot' ? 'image/png' : 'application/gzip', bytes:payload.length}));
      for (let offset = 0; offset < payload.length; offset += 16384) dc.send(payload.slice(offset, offset + 16384));
      dc.send(JSON.stringify({type:'done', bytes:payload.length}));
    };
  };
  await peer.setRemoteDescription({type:'offer', sdp:command.offer});
  await peer.setLocalDescription(await peer.createAnswer());
  await gathered(peer);
  return new Response(JSON.stringify({accepted:true, answer:peer.localDescription.sdp}));
};
'''

RUN = r'''async () => {
  acModal = document.querySelector('#modal');
  await Promise.race([acRun(window.fixture.cmd), new Promise((_, reject) => setTimeout(() => reject(new Error('transfer timeout')), 12000))]);
  const shot = document.querySelector('[data-ac-shot]');
  if (window.fixture.cmd === 'shot' && artifact) await shot.decode();
  const bytes = artifact ? new Uint8Array(await artifact.arrayBuffer()) : new Uint8Array();
  const result = {
    status:document.querySelector('[data-ac-status]').textContent,
    payload:btoa(Array.from(bytes, b => String.fromCharCode(b)).join('')),
    width:shot.naturalWidth,
    log:document.querySelector('[data-ac-out]').textContent,
    download:document.querySelector('[data-ac-dl]').hidden === false,
    enabled:document.querySelector('[data-ac-run]').disabled === false,
    connected:!!selected,
  };
  if (peer) peer.close();
  return result;
}'''


class AdminBrowserInterop(unittest.TestCase):
    def test_png_and_gzip_over_real_datachannels(self):
        template = TEMPLATE.read_text()
        start = template.index('  let acModal = null;')
        script = template[start:template.index('  function closeAdminCmd()', start)]
        log = bytearray(b'a' * 40000)
        log[CHUNK - 15] = ord('{')
        compressed = gzip.compress(bytes(log), compresslevel=0)
        self.assertEqual(compressed[CHUNK], ord('{'))
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            try:
                for cmd, target, payload in [('shot', 'mini', png_fixture()), ('shot', 'desk', png_fixture()), ('log', '', compressed)]:
                    with self.subTest(cmd=cmd, target=target):
                        page = browser.new_page()
                        page.set_content(f'''<div id="modal" data-agent-id="interop-agent">
                          <button data-ac-run></button><span data-ac-status></span><div data-ac-fill></div>
                          <pre data-ac-out></pre><img data-ac-shot hidden><a data-ac-dl hidden></a>
                          <input data-ac-target value="{target}"><input data-ac-days value="1"><input data-ac-dates>
                        </div>''')
                        page.evaluate('(fixture) => { window.fixture = fixture; }', {'cmd':cmd, 'target':target, 'payload':base64.b64encode(payload).decode()})
                        page.add_script_tag(content=SETUP + script)
                        result = page.evaluate(RUN)
                        self.assertEqual(base64.b64decode(result['payload']), payload)
                        self.assertTrue(result['enabled'])
                        self.assertTrue(result['connected'])
                        self.assertEqual(result['status'], '✔ 截图完成' if cmd == 'shot' else '✔ 日志完成')
                        if cmd == 'shot':
                            self.assertEqual(result['width'], 1)
                        else:
                            self.assertTrue(result['download'])
                            self.assertEqual(result['log'], bytes(log[:20000]).decode() + '\n…[40000 B]')
                        page.close()
            finally:
                browser.close()


if __name__ == '__main__':
    unittest.main()
