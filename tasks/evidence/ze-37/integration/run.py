import os,sys,json,subprocess,time,pathlib
p=pathlib.Path('/tmp/ze-37-integration'); name=sys.argv[1]; cmd=sys.argv[2:]; env=dict(os.environ,CARGO_TARGET_DIR='/Users/aghatage/Documents/code/zeppelin-embed/target/ze37-integration'); start=time.time()
with (p/(name+'.log')).open('wb') as out: result=subprocess.run(cmd,stdout=out,stderr=subprocess.STDOUT,env=env)
record=dict(name=name,argv=cmd,workdir=os.getcwd(),target=env['CARGO_TARGET_DIR'],exit_code=result.returncode,seconds=time.time()-start)
(p/(name+'.json')).write_text(json.dumps(record,indent=2)+'\n');print(json.dumps(record));print((p/(name+'.log')).read_text()[-3000:]);sys.exit(result.returncode)
