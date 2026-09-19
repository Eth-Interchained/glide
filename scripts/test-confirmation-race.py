#!/usr/bin/env python3
"""Real CLI / qcow2 confirmation race regression. Requires QEMU and GLIDE_TEST_ISO."""
import json, os, pathlib, pty, select, subprocess, tempfile, time
binary=str(pathlib.Path(__file__).resolve().parents[1]/'target/debug/glide')
iso=os.environ['GLIDE_TEST_ISO']
with tempfile.TemporaryDirectory(prefix='glide-confirm-') as root:
    def cli(*args):
        p=subprocess.run([binary,'--root',root,*args],capture_output=True,text=True,timeout=30)
        assert p.returncode==0, p.stderr
        return p.stdout
    def create():
        return json.loads(cli('--json','create','--name','same-name','--iso',iso,'--arch','x86_64','--disk','1G'))['config']
    first=create()
    master,slave=pty.openpty()
    pending=subprocess.Popen([binary,'--root',root,'delete','same-name'],stdin=slave,stdout=slave,stderr=slave)
    os.close(slave)
    try:
        text=b'';deadline=time.monotonic()+10
        while b'to confirm:' not in text:
            assert time.monotonic()<deadline,'confirmation prompt timeout'
            if select.select([master],[],[],0.2)[0]:text+=os.read(master,8192)
        assert first['id'].encode() in text, 'prompt must identify immutable target UUID'
        cli('remove',first['id'])
        replacement=create()
        assert replacement['id']!=first['id']
        os.write(master,b'same-name\n')
        code=pending.wait(timeout=10)
        assert code!=0,'old confirmation must not delete replacement'
        still=json.loads(cli('status',replacement['id']))
        assert pathlib.Path(still['config']['disk']).is_file()
        assert pathlib.Path(first['disk']).is_file(),'remove must retain original disk'
        print('PASS: confirmed UUID disappeared; replacement VM and both disks survived.')
    finally:
        if pending.poll() is None:pending.kill();pending.wait()
        os.close(master)
