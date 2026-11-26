//! Scan physical gaps in an RPF index. Read-only Win64 mapping, bounded decoder workspace.
use std::fs::File;
use std::io::{self, BufRead};
use std::os::windows::io::AsRawHandle;
use std::ffi::c_void;
use zlib_rs::{Inflate, InflateFlush, Status};

#[link(name="kernel32")]
unsafe extern "system" {
    fn CreateFileMappingW(f: *mut c_void, sa: *mut c_void, protect: u32, hi: u32, lo: u32, name: *const u16) -> *mut c_void;
    fn MapViewOfFile(m: *mut c_void, access: u32, hi: u32, lo: u32, size: usize) -> *const u8;
    fn UnmapViewOfFile(base: *const u8) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

fn audit(data: &[u8], intervals: impl BufRead) -> io::Result<()> {
    let mut state = Inflate::new(true, 15);
    let mut out = vec![0; 65536];
    let (mut cursor, mut scanned, mut hits) = (0, 0, 0);
    for line in intervals.lines().chain(std::iter::once(Ok(format!("{} {}",data.len(),data.len())))) {
        let line = line?;
        let Some((start,end)) = line.split_once(' ') else { return Err(io::Error::other("bad interval")) };
        let number = |s:&str| s.parse::<usize>().map_err(io::Error::other);
        let (start,end)=(number(start)?,number(end)?);
        if end<start || end>data.len() { return Err(io::Error::other("extent outside archive")); }
        let gap_end=start;
        scanned += gap_end.saturating_sub(cursor);
        while cursor<gap_end {
            let at=cursor;
            cursor+=1;
            let cmf=data[at];
            if cmf==b'R' && data[at..].starts_with(b"RDS5") { println!("rds5 {at}"); }
            if cmf & 15 != 8 || cmf>0x78 || at+2>=data.len() { continue; }
            let flg=data[at+1];
            if flg & 32 != 0 || ((cmf as u16)*256+flg as u16)%31!=0 || data[at+2]&6==6 {continue;}
            state.reset(true);
            loop {
                let input=state.total_in() as usize;
                let decoded=state.total_out();
                let result=state.decompress(&data[at+input..], &mut out, InflateFlush::NoFlush);
                match result {
                    Ok(Status::StreamEnd) => {
                        let bytes=state.total_in() as usize;
                        if state.total_out()>0 {
                            println!("zlib {at} {bytes} {}",state.total_out());
                            hits+=1;
                        }
                        cursor=at+bytes;
                        break;
                    }
                    Ok(Status::Ok) if state.total_in() as usize!=input || state.total_out()!=decoded => {}
                    _ => break
                }
            }
        }
        cursor=cursor.max(end);
    }
    eprintln!("scanned={scanned} zlib={hits}");
    Ok(())
}

fn main()->io::Result<()> {
    let mut args=std::env::args_os().skip(1);
    let file=File::open(args.next().ok_or(io::Error::other("archive required"))?)?;
    let ranges=File::open(args.next().ok_or(io::Error::other("intervals required"))?)?;
    let len=usize::try_from(file.metadata()?.len()).map_err(io::Error::other)?;
    if len==0 {return Ok(())}
    // Map the source file read-only; Windows can evict clean pages as it scans.
    unsafe {
        let map=CreateFileMappingW(file.as_raw_handle(),std::ptr::null_mut(),2,0,0,std::ptr::null());
        if map.is_null(){return Err(io::Error::last_os_error())}
        let base=MapViewOfFile(map,4,0,0,0);
        let error=io::Error::last_os_error();
        CloseHandle(map);
        if base.is_null(){return Err(error)}
        let result=audit(std::slice::from_raw_parts(base,len),io::BufReader::new(ranges));
        UnmapViewOfFile(base);
        result
    }
}
