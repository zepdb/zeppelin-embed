import subprocess,sys,pathlib,json,datetime,time
name=sys.argv[1]; cmd=sys.argv[2:]; dst=pathlib.Path('/tmp/ze-121-evidence')
start=datetime.datetime.now(datetime.timezone.utc).isoformat(); tick=time.monotonic()
p=subprocess.run(cmd,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
(dst/(name+'.log')).write_bytes(p.stdout)
(dst/(name+'.json')).write_text(json.dumps({'command':cmd,'cwd':str(pathlib.Path.cwd()),'started_utc':start,'exit':p.returncode,'elapsed_seconds':time.monotonic()-tick},indent=2)+'\n')
sys.stdout.buffer.write(p.stdout); print('RECORDED_EXIT='+str(p.returncode));sys.exit(p.returncode)
