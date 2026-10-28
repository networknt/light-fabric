#!/usr/bin/env python3
"""Local baseline packaging. Rust bootstrap owns configuration admission."""
import argparse, datetime, hashlib, json, os, re, shutil, stat, subprocess, sys, tempfile, zipfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[3]
PROFILE=ROOT/"apps/light-lambda/package-profile.json"
MAX_ZIP_BYTES=250*1024*1024

def digest(data): return hashlib.sha256(data).hexdigest()
def file_id(path):
    p=Path(path);return {"path":str(p.resolve()),"bytes":p.stat().st_size,"sha256":digest(p.read_bytes())}
def run(args,**kwargs):
    return subprocess.run([*map(str,args)],check=True,**kwargs)
def epoch():
    value=os.environ.get("SOURCE_DATE_EPOCH")
    if value is None: value=run(["git","show","-s","--format=%ct","HEAD"],cwd=ROOT,capture_output=True,text=True).stdout.strip()
    value=int(value)
    if value<315532800 or value>4354819199: raise ValueError("SOURCE_DATE_EPOCH must fit ZIP timestamps (1980-2107)")
    return value

def canonical_zip(path,files,modes,stamp):
    with zipfile.ZipFile(path,"w",compression=zipfile.ZIP_DEFLATED,compresslevel=9) as archive:
        for name in sorted(files):
            info=zipfile.ZipInfo(name,datetime.datetime.fromtimestamp(stamp,datetime.timezone.utc).timetuple()[:6]);info.create_system=3
            info.external_attr=(stat.S_IFREG|modes.get(name,0o644))<<16;info.compress_type=zipfile.ZIP_DEFLATED
            archive.writestr(info,files[name],compresslevel=9)

def safe_entries(archive):
    seen=set();total=0
    for info in archive.infolist():
        path=Path(info.filename)
        if info.filename in seen or path.is_absolute() or not path.parts or any(x in ('..','.') for x in path.parts) or '\\' in info.filename or info.filename.endswith('/') or path.as_posix()!=info.filename:
            raise ValueError("invalid or duplicate ZIP entry")
        mode=info.external_attr>>16
        if not stat.S_ISREG(mode): raise ValueError("ZIP entries must be regular files")
        total+=info.file_size
        if total>MAX_ZIP_BYTES: raise ValueError("local package size limit exceeded")
        seen.add(info.filename)
    return seen

def validate(bootstrap,directory,*,allow_logger_set=False):
    env={k:v for k,v in os.environ.items() if not k.startswith(('AWS_','LIGHT_LAMBDA_'))}
    env['AWS_EC2_METADATA_DISABLED']='true'
    # Test endpoint admission still requires the existing explicit process flag.
    if os.environ.get('LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS')=='1':env['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS']='1'
    # Explicit tooling opt-in, never inherited from a caller's ambient gate.
    # A layer cannot authorize mutation; the runtime still needs the same gate.
    if allow_logger_set:
        env['LIGHT_LAMBDA_ALLOW_LOGGER_SET']='1'
        env.setdefault('RUST_LOG','info')
    with tempfile.TemporaryDirectory(prefix='light-lambda-validate-') as work:
        p=subprocess.run([str(Path(bootstrap).resolve()),'validate-config','--config-dir',str(Path(directory).resolve())],cwd=work,env=env,text=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=30)
    try: report=json.loads(p.stdout)
    except ValueError: raise ValueError('bootstrap did not produce a configuration validation report')
    if p.returncode or not report.get('valid'): raise ValueError('Rust Init/configuration rejected: '+json.dumps(report.get('errors',[])))
    return report

def layer(source,output,bootstrap,*,allow_logger_set=False):
    source=Path(source).resolve();output=Path(output).resolve()
    if not source.is_dir():raise ValueError('layer source must be a directory')
    if output.exists():raise ValueError('layer output already exists; choose a fresh path')
    if source==output.parent or source in output.parents:raise ValueError('layer output must be outside source')
    files={}
    for path in sorted(source.rglob('*')):
        if path.is_symlink():raise ValueError('layer symlinks are not supported')
        if path.is_dir():continue
        if not path.is_file():raise ValueError('layer entries must be regular files')
        name=path.relative_to(source).as_posix()
        if name=='light-lambda-layer.json':raise ValueError('source contains a generated layer manifest')
        files[name]=path.read_bytes()
    with tempfile.TemporaryDirectory(prefix='light-lambda-layer-') as work:
        staging=Path(work)
        for name,data in files.items():
            p=staging/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(data)
        report=validate(bootstrap,staging,allow_logger_set=allow_logger_set);stamp=epoch()
        manifest={'schema':'light-lambda-layer/v1','files':[{'path':n,'sha256':digest(files[n])} for n in sorted(files)],'configDigest':report['configDigest'],'createdAt':datetime.datetime.fromtimestamp(stamp,datetime.timezone.utc).isoformat().replace('+00:00','Z')}
        files['light-lambda-layer.json']=(json.dumps(manifest,sort_keys=True,separators=(',',':'))+'\n').encode();(staging/'light-lambda-layer.json').write_bytes(files['light-lambda-layer.json'])
        if validate(bootstrap,staging,allow_logger_set=allow_logger_set)['configDigest']!=report['configDigest']:raise ValueError('manifest unexpectedly changed configuration digest')
        output.parent.mkdir(parents=True,exist_ok=True);canonical_zip(output,files,{},stamp)
    return {'layer':file_id(output),'files':len(files),'configDigest':report['configDigest'],'handlers':report['handlers'],'sourceDateEpoch':stamp}

def check_zip(package):
    package=Path(package)
    with zipfile.ZipFile(package) as archive:
        if safe_entries(archive)!={'bootstrap'}:raise ValueError('baseline ZIP must contain exactly bootstrap')
        info=archive.getinfo('bootstrap')
        if stat.S_IMODE(info.external_attr>>16)!=0o755:raise ValueError('bootstrap mode must be 0755')
        data=archive.read(info)
    if data[:6]!=b'\x7fELF\x02\x01' or int.from_bytes(data[18:20],'little')!=62:raise ValueError('bootstrap must be little-endian x86-64 ELF')
    with tempfile.TemporaryDirectory(prefix='light-lambda-elf-') as work:
        binary=Path(work)/'bootstrap';binary.write_bytes(data)
        dynamic=run(['readelf','-d',binary],capture_output=True,text=True).stdout
        versions=run(['readelf','--version-info',binary],capture_output=True,text=True).stdout
        description=run(['file','-b',binary],capture_output=True,text=True).stdout.strip()
    needed=re.findall(r'\(NEEDED\).*\[(.*?)\]',dynamic)
    if set(needed)-{'libc.so.6','libm.so.6','libgcc_s.so.1','ld-linux-x86-64.so.2'}:raise ValueError('bootstrap has unsupported dynamic dependencies: '+repr(needed))
    glibc=[tuple(map(int,v)) for v in re.findall(r'GLIBC_(\d+)\.(\d+)',versions)];maximum=max(glibc,default=None)
    if maximum and maximum>(2,34):raise ValueError('bootstrap requires GLIBC above 2.34')
    return {'profile':'baseline-bootstrap-only-v1','zip':file_id(package),'bootstrap':{'sha256':digest(data),'bytes':len(data),'mode':'0755','file':description,'needed':needed,'maximumGlibc':'.'.join(map(str,maximum)) if maximum else None},'supportedHandlers':json.loads(PROFILE.read_text())['supportedHandlers'],'conditionalHandlers':json.loads(PROFILE.read_text()).get('conditionalHandlers',{}),'workerDependencies':[]}

def extract_layer(package,directory):
    directory=Path(directory);directory.mkdir(parents=True,exist_ok=False)
    with zipfile.ZipFile(package) as archive:
        names=safe_entries(archive)
        if 'light-lambda-layer.json' not in names:raise ValueError('layer manifest missing')
        manifest=json.loads(archive.read('light-lambda-layer.json'))
        if manifest.get('schema')!='light-lambda-layer/v1' or not re.fullmatch('[0-9a-fA-F]{64}',str(manifest.get('configDigest',''))) or not isinstance(manifest.get('createdAt'),str) or not manifest['createdAt']:
            raise ValueError('invalid layer manifest metadata')
        entries=manifest['files'];listed={x['path'] for x in entries}
        if len(entries)!=len(listed) or names!=listed|{'light-lambda-layer.json'}:raise ValueError('layer contents differ from manifest')
        for entry in entries:
            if digest(archive.read(entry['path']))!=entry['sha256'].lower():raise ValueError('layer content digest mismatch')
        for name in names:
            path=directory/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(archive.read(name));path.chmod(0o644)
    return manifest

def build(output,tool,entry="bootstrap"):
    if entry not in ("bootstrap", "validator-in-process"):raise ValueError("unsupported package entry")
    output=Path(output).resolve();output.mkdir(parents=True,exist_ok=False);tool=Path(tool or shutil.which('cargo-lambda') or '').resolve()
    version=run([tool,'lambda','--version'],capture_output=True,text=True).stdout.strip()
    if not version.startswith('cargo-lambda 1.9.2 '):raise ValueError('cargo-lambda must be pinned to 1.9.2')
    stamp=epoch();head=run(['git','rev-parse','HEAD'],cwd=ROOT,capture_output=True,text=True).stdout.strip()
    candidates=run(['git','ls-files','-z','--cached','--others','--exclude-standard'],cwd=ROOT,capture_output=True).stdout.decode().split('\0')
    inputs={name:digest((ROOT/name).read_bytes()) for name in sorted(set(candidates)) if name and (ROOT/name).is_file() and (name.endswith(('.rs','.toml','.lock')) or name.startswith('apps/light-lambda/config/'))}
    (output/'source-inputs.json').write_text(json.dumps(inputs,indent=2)+'\n')
    shutil.copyfile(__file__,output/'build-tool-source.py')
    with (output/'dirty-tracked.patch').open('w') as f:run(['git','diff','--binary','--','Cargo.toml','Cargo.lock',':(glob)**/*.rs',':(glob)**/Cargo.toml','apps/light-lambda/config'],cwd=ROOT,stdout=f)
    ids=[];commands=[];build_environments=[]
    for number in (1,2):
        target=output/f'target-{number}';destination=output/f'lambda-{number}'
        flags=f'--remap-path-prefix={Path.home()}=~ --remap-path-prefix={ROOT}=. --remap-path-prefix={target}=/target'
        env={**os.environ,'SOURCE_DATE_EPOCH':str(stamp),'RUSTFLAGS':flags,'CARGO_INCREMENTAL':'0','CARGO_TARGET_DIR':str(target),'CARGO_BUILD_JOBS':os.environ.get('CARGO_BUILD_JOBS','4')}
        args=[tool,'lambda','build','--locked','--offline','--profile','lambda','-p','light-lambda','--bin',entry,'--target','x86_64-unknown-linux-gnu.2.34','--output-format','zip','--lambda-dir',destination]
        commands.append(list(map(str,args)));build_environments.append({k:env[k] for k in ['SOURCE_DATE_EPOCH','RUSTFLAGS','CARGO_INCREMENTAL','CARGO_TARGET_DIR','CARGO_BUILD_JOBS']})
        with (output/f'build-{number}.log').open('w') as log:run(args,cwd=ROOT,env=env,stdout=log,stderr=subprocess.STDOUT)
        archives=list(destination.rglob('*.zip'))
        if len(archives)!=1:raise ValueError('cargo-lambda did not produce one bootstrap ZIP')
        with zipfile.ZipFile(archives[0]) as archive:data=archive.read('bootstrap')
        binary=output/f'bootstrap-{number}';binary.write_bytes(data);binary.chmod(0o755)
        package=output/f'bootstrap-{number}.zip';canonical_zip(package,{'bootstrap':data},{'bootstrap':0o755},stamp);ids.append(check_zip(package))
    if ids[0]['bootstrap']['sha256']!=ids[1]['bootstrap']['sha256'] or ids[0]['zip']['sha256']!=ids[1]['zip']['sha256']:raise ValueError('clean builds are not byte-identical')
    if any(digest((ROOT/name).read_bytes())!=value for name,value in inputs.items()):raise ValueError('Rust build inputs changed during reproducibility run')
    shutil.copyfile(output/'bootstrap-1.zip',output/'bootstrap.zip');shutil.copyfile(output/'bootstrap-1',output/'bootstrap');(output/'bootstrap').chmod(0o755)
    result={'entry':entry,'profile':json.loads(PROFILE.read_text()),'sourceHead':head,'dirtySourceInputs':file_id(output/'source-inputs.json'),'dirtyTrackedPatch':file_id(output/'dirty-tracked.patch'),'sourceDateEpoch':stamp,'cargoLambda':{'version':version,**file_id(tool)},'rustc':run(['rustc','-vV'],capture_output=True,text=True).stdout,'zig':run(['zig','version'],capture_output=True,text=True).stdout.strip(),'commands':commands,'buildEnvironments':build_environments,'buildToolSource':file_id(output/'build-tool-source.py'),'target':'x86_64-unknown-linux-gnu.2.34','cleanIndependentTargets':True,'reproducibleBootstrap':True,'reproducibleZip':True,'builds':ids,'package':check_zip(output/'bootstrap.zip')}
    (output/'package-manifest.json').write_text(json.dumps(result,indent=2)+'\n')
    # These directories were created by this build in a fresh output root only.
    # Retain logs, sources/digests and final artifacts, not redundant compile caches.
    for number in (1,2):shutil.rmtree(output/f'target-{number}');shutil.rmtree(output/f'lambda-{number}')
    return result

def main():
    p=argparse.ArgumentParser();sub=p.add_subparsers(dest='command',required=True)
    s=sub.add_parser('layer');s.add_argument('source');s.add_argument('output');s.add_argument('--bootstrap',required=True);s.add_argument('--allow-logger-set',action='store_true',help='validate bounded LoggerSet using its explicit trusted startup gate; deployment must opt in separately')
    s=sub.add_parser('check-zip');s.add_argument('package')
    s=sub.add_parser('extract-layer');s.add_argument('package');s.add_argument('directory')
    s=sub.add_parser('build');s.add_argument('output');s.add_argument('--cargo-lambda',default=os.environ.get('CARGO_LAMBDA'));s.add_argument('--entry',choices=['bootstrap','validator-in-process'],default='bootstrap')
    a=p.parse_args()
    if a.command=='layer':result=layer(a.source,a.output,a.bootstrap,allow_logger_set=a.allow_logger_set)
    elif a.command=='check-zip':result=check_zip(a.package)
    elif a.command=='extract-layer':result=extract_layer(a.package,a.directory)
    else:result=build(a.output,a.cargo_lambda,a.entry)
    print(json.dumps(result,indent=2))
if __name__=='__main__':
    try:main()
    except (ValueError,OSError,subprocess.CalledProcessError,subprocess.TimeoutExpired,zipfile.BadZipFile,KeyError) as error:print(str(error),file=sys.stderr);sys.exit(1)
