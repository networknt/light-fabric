#!/usr/bin/env python3
"""Static-only, separate extension layer producer. Never executes either input ELF."""
import argparse,hashlib,importlib.util,json,os,stat,zipfile
from pathlib import Path,PurePosixPath
HERE=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('bridge_baseline_static_checks',HERE/'package_baseline.py')
baseline=importlib.util.module_from_spec(spec);spec.loader.exec_module(baseline)
EXTENSION='extensions/validator-shutdown-bridge'
RECEIPT='validator-shutdown/bridge-manifest.json'
MAX_IMAGE=256*1024*1024

def identity(path):
    path=Path(path)
    if path.is_symlink() or not path.is_file():raise ValueError('ELF input must be a regular non-symlink')
    size=path.stat().st_size
    if not 0<size<=MAX_IMAGE:raise ValueError('ELF input size')
    data=path.read_bytes()
    if len(data)!=size:raise ValueError('ELF input changed')
    return data,{'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()}

def archive(path,entries):
    with zipfile.ZipFile(path,'x',compression=zipfile.ZIP_DEFLATED) as z:
        for name,data,mode in sorted(entries):
            info=zipfile.ZipInfo(name,(1980,1,1,0,0,0));info.external_attr=(stat.S_IFREG|mode)<<16;info.compress_type=zipfile.ZIP_DEFLATED
            z.writestr(info,data)

def validate_elf(path):
    # check_zip only reads ELF/ZIP metadata using readelf; no validate-config/run.
    return baseline.check_zip(path)

def build(parent,extension,out):
    out=Path(out);out.mkdir(parents=True,exist_ok=False)
    parent_bytes,parent_id=identity(parent);extension_bytes,extension_id=identity(extension)
    if parent_id==extension_id:raise ValueError('role images must be distinct')
    archive(out/'bootstrap.zip',[('bootstrap',parent_bytes,0o755)])
    archive(out/'extension-elf-check.zip',[('bootstrap',extension_bytes,0o755)])
    parent_check=validate_elf(out/'bootstrap.zip');extension_check=validate_elf(out/'extension-elf-check.zip')
    manifest={'protocol_version':1,'parent':parent_id,'extension':extension_id,'socket_directory':'/tmp/validator-shutdown-bridge','owner_requested_bytes':224*1024*1024,'extension_requested_bytes':32*1024*1024,'parent_side_envelope_bytes':256*1024*1024}
    raw=(json.dumps(manifest,sort_keys=True,indent=2)+'\n').encode()
    if len(raw)>4096:raise ValueError('manifest size')
    (out/'bridge-manifest.json').write_bytes(raw)
    archive(out/'extension-layer.zip',[(EXTENSION,extension_bytes,0o755),(RECEIPT,raw,0o644)])
    report={'profile':'validator-external-shutdown-layer/v1','parent':parent_id,'extension':extension_id,'manifestSha256':hashlib.sha256(raw).hexdigest(),'layerSha256':hashlib.sha256((out/'extension-layer.zip').read_bytes()).hexdigest(),'parentZipSha256':hashlib.sha256((out/'bootstrap.zip').read_bytes()).hexdigest(),'parentStaticCheck':parent_check,'extensionStaticCheck':extension_check,'nativeExecutionPerformed':False,'memoryPartitionIsExperimental':True,'notMemoryBounds':True}
    (out/'package-report.json').write_text(json.dumps(report,indent=2)+'\n')
    return report

def safe_entries(path):
    with zipfile.ZipFile(path) as z:
        seen=set()
        for info in z.infolist():
            name=info.filename;pure=PurePosixPath(name)
            if info.is_dir():raise ValueError('directory records are not part of this layer profile')
            if pure.is_absolute() or '..' in pure.parts or str(pure)!=name or not name or name in seen:raise ValueError('unsafe/duplicate ZIP path')
            seen.add(name)
            if not stat.S_ISREG(info.external_attr>>16):raise ValueError('non-regular ZIP member')
            if info.file_size>MAX_IMAGE:raise ValueError('ZIP member size')
            yield name,z.read(info),(info.external_attr>>16)&0o777

def compose(data_layer,data_sha,extension_layer,extension_sha,out):
    for path,expected in [(data_layer,data_sha),(extension_layer,extension_sha)]:
        if hashlib.sha256(Path(path).read_bytes()).hexdigest()!=expected:raise ValueError('layer hash mismatch')
    ext=list(safe_entries(extension_layer))
    if {n for n,_,_ in ext}!={EXTENSION,RECEIPT} or len(ext)!=2:raise ValueError('extension layer inventory')
    modes={n:m for n,_,m in ext}
    if modes!={EXTENSION:0o755,RECEIPT:0o644}:raise ValueError('extension layer modes')
    table={n:b for n,b,_ in ext};raw=table[RECEIPT]
    if len(raw)>4096:raise ValueError('manifest size')
    def strict(pairs):
        value={}
        for k,v in pairs:
            if k in value:raise ValueError('duplicate manifest field')
            value[k]=v
        return value
    manifest=json.loads(raw,object_pairs_hook=strict)
    expected_keys={'protocol_version','parent','extension','socket_directory','owner_requested_bytes','extension_requested_bytes','parent_side_envelope_bytes'}
    if not isinstance(manifest,dict) or set(manifest)!=expected_keys or type(manifest['protocol_version']) is not int or manifest['protocol_version']!=1 or manifest['socket_directory']!='/tmp/validator-shutdown-bridge':raise ValueError('manifest profile')
    if any(type(manifest[k]) is not int for k in ['owner_requested_bytes','extension_requested_bytes','parent_side_envelope_bytes']) or (manifest['owner_requested_bytes'],manifest['extension_requested_bytes'],manifest['parent_side_envelope_bytes'])!=(224*1024*1024,32*1024*1024,256*1024*1024):raise ValueError('manifest partition')
    for role in ['parent','extension']:
        pin=manifest[role]
        if not isinstance(pin,dict) or set(pin)!={'bytes','sha256'} or type(pin['bytes']) is not int or not 0<pin['bytes']<=MAX_IMAGE or not isinstance(pin['sha256'],str) or len(pin['sha256'])!=64 or any(c not in '0123456789abcdef' for c in pin['sha256']):raise ValueError('manifest role pin')
    if manifest['extension']!={'bytes':len(table[EXTENSION]),'sha256':hashlib.sha256(table[EXTENSION]).hexdigest()}:raise ValueError('extension image pin')
    data=list(safe_entries(data_layer));entries=data+ext;names=[n for n,_,_ in entries]
    if len(set(names))!=len(names) or any(n.startswith(('extensions/','validator-shutdown/')) for n,_,_ in data):raise ValueError('layer collision')
    out=Path(out);out.mkdir(parents=True,exist_ok=False)
    for name,body,mode in entries:
        path=out/name;path.parent.mkdir(parents=True,exist_ok=True)
        with path.open('xb') as f:f.write(body)
        path.chmod(mode)
    return {'dataLayerSha256':data_sha,'extensionLayerSha256':extension_sha,'manifestSha256':hashlib.sha256(raw).hexdigest(),'files':len(entries),'originalDataEntriesUnchanged':True,'nativeExecutionPerformed':False}

def main():
    parser=argparse.ArgumentParser(description=__doc__);sub=parser.add_subparsers(dest='action',required=True)
    p=sub.add_parser('build');p.add_argument('--parent',type=Path,required=True);p.add_argument('--extension',type=Path,required=True);p.add_argument('--out',type=Path,required=True)
    p=sub.add_parser('compose');p.add_argument('--data-layer',type=Path,required=True);p.add_argument('--data-sha256',required=True);p.add_argument('--extension-layer',type=Path,required=True);p.add_argument('--extension-sha256',required=True);p.add_argument('--out',type=Path,required=True)
    args=parser.parse_args()
    report=build(args.parent,args.extension,args.out) if args.action=='build' else compose(args.data_layer,args.data_sha256,args.extension_layer,args.extension_sha256,args.out)
    print(json.dumps(report,indent=2))
if __name__=='__main__':main()
