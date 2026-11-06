#!/usr/bin/env python3
"""Verify numbered buffers, structured arguments, and guarded helper edits in a real terminal."""
import argparse, fcntl, json, os, pty, re, select, signal, struct, subprocess, tempfile, termios, threading, time
from pathlib import Path

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary',required=True,type=Path)
args=parser.parse_args(); binary=str(args.binary.resolve());
workspace=Path(tempfile.mkdtemp(prefix='helix-programs-'))
helper=workspace/'helper.py'
helper.write_text('''#!/usr/bin/python3
import sys,os,time,json
from pathlib import Path
root=Path(os.environ['PROGRAM_TEST_DIRECTORY'])
(root/'arguments.json').write_text(json.dumps(sys.argv[1:]))
if sys.argv[1]=='slow-upper':
 (root/'started').write_text('ready')
 time.sleep(0.7)
if sys.argv[1] in ['upper','slow-upper']: print(sys.stdin.read().upper(),end='')
''');helper.chmod(0o755)
clipboard=workspace/'clipboard.bin';clipboard.write_bytes(b'')
clipboard_helper=workspace/'clipboard.py'
clipboard_helper.write_text("""import sys,os
from pathlib import Path
path=Path(os.environ['PROGRAM_TEST_DIRECTORY'])/'clipboard.bin'
if sys.argv[1]=='copy': path.write_bytes(sys.stdin.buffer.read())
else: sys.stdout.buffer.write(path.read_bytes())
""")
fixture_config=workspace/'config.toml'
configuration='[keys.normal]\nC-s = ":update"\n'
configuration+='\n[editor.clipboard-provider.custom]\n'
for name,operation in [('yank','paste'),('paste','copy')]:
 configuration+=name+' = { command = "/usr/bin/python3", args = '+json.dumps([str(clipboard_helper),operation])+' }\n'
fixture_config.write_text(configuration)
source=workspace/'first file.txt';source.write_text('Mixed text\n')
other=workspace/'second file.txt';other.write_text('second\n')
master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',30,120,0,0))
env=os.environ.copy();env.update(TERM='xterm-256color',HELIX_CONTROL_CONTEXT='session:helper-test',PROGRAM_TEST_DIRECTORY=str(workspace))
process=subprocess.Popen([binary,'--config',str(fixture_config),str(source),str(other),'--log',str(workspace/'editor.log')],cwd=workspace,env=env,stdin=slave,stdout=slave,stderr=slave,start_new_session=True)
os.close(slave);transcript=bytearray()
def drain():
 while process.poll() is None:
  if select.select([master],[],[],0.02)[0]:
   try: transcript.extend(os.read(master,65536))
   except OSError: break
reader=threading.Thread(target=drain,daemon=True);reader.start()
def keys(value): os.write(master,value.encode());time.sleep(0.12)
def command(value): keys(':'+value+'\r')
def request(value):
 return json.loads(subprocess.check_output([binary,'--control',str(process.pid),value,env['HELIX_CONTROL_CONTEXT']],text=True,timeout=3))
def wait_for(predicate):
 end=time.monotonic()+5
 while time.monotonic()<end:
  if predicate(): return
  if process.poll() is not None: raise AssertionError((process.returncode,transcript[-2000:]))
  time.sleep(0.02)
 raise AssertionError('Timed out waiting for helper')
def slow():
 (workspace/'started').unlink(missing_ok=True)
 keys('%');command('pipe-program -- '+str(helper)+' slow-upper');wait_for(lambda:(workspace/'started').exists())
try:
 endpoint=Path(env.get('XDG_CACHE_HOME',str(Path.home()/'.cache')))/'helix/control'/f'{process.pid}.sock'
 wait_for(endpoint.exists)
 assert request('buffer:2')['outcome']=='moved'
 switched=request('buffer:1');assert switched['outcome']=='moved' and switched['before']!=switched['after'],switched
 assert request('buffer:9')['outcome']=='blocked'
 keys(':echo pending');assert request('buffer:2')['outcome']=='blocked';keys('\x1b')
 literal="file a's $(untouched).rb"
 command('run-program -- '+str(helper)+' arguments "'+literal+'"')
 wait_for(lambda:(workspace/'arguments.json').exists())
 assert json.loads((workspace/'arguments.json').read_text())==['arguments',literal]
 keys('%');command('pipe-program -- '+str(helper)+' upper');time.sleep(0.4);keys('\x13')
 assert source.read_text()=='MIXED TEXT\n',source.read_text()
 keys('%cMiXeD text\x1b');keys('\x13');original=source.read_text()
 slow();keys('iNEW\x1b');time.sleep(0.8);keys('\x13')
 assert source.read_text()=='NEW'+original,source.read_text()
 original=source.read_text();slow();assert request('buffer:2')['outcome']=='moved';time.sleep(0.8)
 assert other.read_text()=='second\n';request('buffer:1');keys('\x13');assert source.read_text()==original
 slow();keys(':echo keep-this-prompt');time.sleep(0.8)
 assert request('buffer:2')['outcome']=='blocked';keys('\x1b');keys('\x13');assert source.read_text()==original
 slow();keys('h');time.sleep(0.8);keys('\x13');assert source.read_text()==original
 slow();keys('i');time.sleep(0.8);keys('\x1b');keys('\x13');assert source.read_text()==original
 command('vsplit');slow();command('quit');time.sleep(0.8);keys('\x13');assert source.read_text()==original
 clipboard.write_bytes(b'clipboard-sentinel')
 (workspace/'started').unlink(missing_ok=True)
 keys('%');command('pipe-program --clipboard -- '+str(helper)+' slow-upper');wait_for(lambda:(workspace/'started').exists())
 keys('iNEW\x1b');time.sleep(0.8)
 assert clipboard.read_bytes()==b'clipboard-sentinel','A stale result changed the clipboard'
 print('PASS: failure extraction and clipboard results share the editor-context guard.')
 command('quit-all!');process.wait(timeout=3)
 print('PASS: numbered buffers, missing position, pending prompt, literal argv, helper edit, stale buffer/selection/prompt guards.')
finally:
 if process.poll() is None: process.terminate();process.wait(timeout=3)
 reader.join(timeout=1);os.close(master)
 (workspace/'transcript.vt').write_bytes(transcript)
 print('Evidence:',workspace)
