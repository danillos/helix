#!/usr/bin/env python3
"""Verify native editor splits through the editor control endpoint in a private terminal."""
import argparse, fcntl, json, os, pty, select, socket, struct, subprocess, tempfile, termios, threading, time
from pathlib import Path

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary',required=True,type=Path)
binary=str(parser.parse_args().binary.resolve())
assert json.loads(subprocess.check_output([binary,'--control','probe-split'],text=True))=={'split_creation':True}
workspace=Path(tempfile.mkdtemp(prefix='helix-control-splits-'))
source=workspace/'split.txt';original='original text\n';source.write_text(original)
config=workspace/'config.toml';config.write_text('')
master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',36,140,0,0))
env=os.environ.copy();env.update(TERM='xterm-256color',HELIX_CONTROL_CONTEXT='session:split-test')
process=subprocess.Popen([binary,'--config',str(config),str(source),'--log',str(workspace/'editor.log')],cwd=workspace,env=env,stdin=slave,stdout=slave,stderr=slave,start_new_session=True)
os.close(slave);transcript=bytearray()
def drain():
 while process.poll() is None:
  if select.select([master],[],[],0.02)[0]:
   try:transcript.extend(os.read(master,65536))
   except OSError:break
reader=threading.Thread(target=drain,daemon=True);reader.start()
def keys(value):os.write(master,value.encode());time.sleep(.12)
def command(value):keys(':'+value+'\r')
def request(value):
 return json.loads(subprocess.check_output([binary,'--control',str(process.pid),value,env['HELIX_CONTROL_CONTEXT']],text=True,timeout=3))
endpoint=Path(env.get('XDG_CACHE_HOME',str(Path.home()/'.cache')))/'helix/control'/f'{process.pid}.sock'
def raw_split(identity,deadline):
 with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as stream:
  stream.settimeout(2);stream.connect(str(endpoint));reader=stream.makefile('rb')
  hello=json.loads(reader.readline());assert hello['split_creation']
  stream.sendall((json.dumps({'operation':'split','instance':hello['instance'],'id':identity,'direction':'right','deadline_ms':deadline})+'\n').encode())
  return json.loads(reader.readline())
try:
 deadline=time.monotonic()+5
 while not endpoint.exists() and time.monotonic()<deadline:time.sleep(.02)
 assert endpoint.exists(),transcript[-2000:]
 assert request('left')['outcome']=='edge'
 assert raw_split('expired-split',0)['outcome']=='not_applied'
 assert request('right')['outcome']=='edge'
 created=raw_split('same-split',int(time.time()*1000)+150)
 assert created['outcome']=='moved' and created['before']!=created['after'],created
 assert raw_split('same-split',int(time.time()*1000)+150)==created
 left=request('left');assert left['outcome']=='moved' and left['after']==created['before'],left
 assert request('left')['outcome']=='edge'
 assert request('right')['after']==created['after']
 assert request('right')['outcome']=='edge'
 for pending in [':echo unfinished','g','2']:
  keys(pending);assert request('split:down')['outcome']=='blocked';keys('\x1b')
  assert request('down')['outcome']=='edge'
 keys('\x1b[O');assert request('split:down')['outcome']=='stale';keys('\x1b[I')
 below=request('split:down');assert below['outcome']=='moved'
 assert request('up')['after']==below['before']
 assert request('up')['outcome']=='edge'
 assert request('down')['after']==below['after']
 assert request('down')['outcome']=='edge'
 command('quit');command('quit')
 for mode,enter in [('normal',''),('insert','i'),('select','v')]:
  for direction in ['right','down']:
   keys('gggh'+enter)
   result=request('split:'+direction);assert result['outcome']=='moved',result
   keys('iMARK\x1b');command('write')
   assert source.read_text()=='MARK'+original,(mode,direction,source.read_text())
   keys('u');command('write');assert source.read_text()==original
   command('quit')
 assert source.read_text()==original
 command('quit-all!');process.wait(timeout=3)
 print('PASS: right/down geometry; same-buffer views; Normal/Insert/Select use native Normal-mode splitting; unchanged text and undo.')
 print('PASS: duplicate request creates one view; expiry, prompts, pending keys/counts, and focus loss prevent extra splits.')
finally:
 if process.poll() is None:process.terminate();process.wait(timeout=3)
 reader.join(timeout=1);os.close(master)
 (workspace/'transcript.vt').write_bytes(transcript)
 print('Evidence:',workspace)
