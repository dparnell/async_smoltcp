#![allow(unsafe_code)]
#![allow(unused)]

//use super::smol_stack::{Blob};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::{Error};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::vec::Vec;
use std::time::Duration;
use std::isize;
use std::ops::Deref;
use std::slice;

//static ERR_WOULD_BLOCK: u32 = 1;

pub enum VirtualTunReadError {
    WouldBlock
}

#[derive(Debug)]
pub enum VirtualTunWriteError {
    NoData
}

pub type OnVirtualTunRead = Arc<dyn Fn(&mut [u8]) -> Result<usize,VirtualTunReadError> + Send + Sync>;
//pub type OnVirtualTunWrite = Arc<dyn Fn(&dyn FnOnce(&mut [u8]) , usize) -> Result<(), VirtualTunWriteError> + Send + Sync>;
pub type OnVirtualTunWrite = Arc<
    dyn Fn(&mut dyn FnMut(&mut [u8]), usize) -> Result<(), VirtualTunWriteError> + Send + Sync,
>;

#[derive(Clone)]
pub struct VirtualTunInterface {
    mtu: usize,
    //has_data: Arc<(Mutex<()>, Condvar)>,
    on_virtual_tun_read: OnVirtualTunRead,
    on_virtual_tun_write: OnVirtualTunWrite,
}

/*
fn copy_slice(dst: &mut [u8], src: &[u8]) -> usize {
    let mut c = 0;
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d = *s;
        c += 1;
    }
    c 
}
*/

impl<'a> VirtualTunInterface {
    pub fn new(
        _name: &str,
        on_virtual_tun_read: OnVirtualTunRead,
        on_virtual_tun_write: OnVirtualTunWrite,
        //has_data: Arc<(Mutex<()>, Condvar)>,
        mtu: usize
    ) -> smoltcp::Result<VirtualTunInterface> {
        //let mtu = 1500; //??
        Ok(VirtualTunInterface {
            mtu: mtu,
            //has_data: has_data,
            on_virtual_tun_read: on_virtual_tun_read,
            on_virtual_tun_write: on_virtual_tun_write,
        })
    }
}

impl<'d> Device<'d> for VirtualTunInterface {
    type RxToken = RxToken;
    type TxToken = TxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut d = DeviceCapabilities::default();
        d.max_transmission_unit = self.mtu;
        d
    }

    fn receive(&'d mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        let mut buffer = vec![0; self.mtu];
        let r = (self.on_virtual_tun_read)(buffer.as_mut_slice());
        match r {
            Ok(size) => {
                buffer.resize(size, 0);
                let rx = RxToken {
                    lower: Rc::new(RefCell::new(self.clone())),
                    buffer,
                    size
                };
                let tx = TxToken {
                    lower: Rc::new(RefCell::new(self.clone())),
                };
                Some((rx, tx))
            },
            //Simulates a tun/tap device that returns EWOULDBLOCK
            Err(VirtualTunReadError::WouldBlock) => None,
            //TODO: do not panic
            Err(_) => panic!("unknown error on virtual tun receive"),
        }
    }

    fn transmit(&'d mut self) -> Option<Self::TxToken> {
        Some(TxToken {
            lower: Rc::new(RefCell::new(self.clone())),
        })
    }

    fn medium(&self) -> Medium {
        Medium::Ip
    }
}

#[doc(hidden)]
pub struct RxToken {
    lower: Rc<RefCell<VirtualTunInterface>>,
    buffer: Vec<u8>,
    size: usize
}

impl phy::RxToken for RxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        //println!("rx consume");
        let mut lower = self.lower.as_ref().borrow_mut();
        let r = f(&mut self.buffer[..]);
        
        /*
        match &r {
            Ok(_) => println!("ok"),
            Err(_) => println!("err")
        }
        */

        //let (mutex, has_data_condition_variable) = &*lower.has_data.clone();
        //has_data_condition_variable.notify_one();
        r
    }
}

//https://stackoverflow.com/a/66579120/5884503
//https://users.rust-lang.org/t/storing-the-return-value-from-an-fn-closure/57386/3?u=guerlando
trait CallOnceSafe<R> {
    fn call_once_safe(&mut self, x: &mut [u8]) -> smoltcp::Result<R>;
}

impl<R, F: FnOnce(&mut [u8]) -> smoltcp::Result<R>> CallOnceSafe<R> for Option<F> {
    fn call_once_safe(&mut self, x: &mut [u8]) -> smoltcp::Result<R> {
        // panics if called more than once - but A::consume() calls it only once
        let func = self.take().unwrap();
        func(x)
    }
}

#[doc(hidden)]
pub struct TxToken {
    lower: Rc<RefCell<VirtualTunInterface>>,
}

impl<'a> phy::TxToken for TxToken {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        let mut lower = self.lower.as_ref().borrow_mut();
        //let mut buffer = vec![0; len];
        //let result = f(&mut buffer);
        let mut r: Option<smoltcp::Result<R>> = None;
        /*
        let _result = (lower.on_virtual_tun_write)(&|b: &mut [u8]| {
            r = Some(f(b));
        }, len);
        */
        let mut f = Some(f);

        match (lower.on_virtual_tun_write)(&mut |x| {
            r = Some(f.call_once_safe(x))
        }, len) {
            Ok(()) => {

            },
            Err(_) => {
                panic!("virtual tun receive unknown error");
            }
        }
        //let result = lower.send(f, len);



        /*
        let packets_from_inside = &*lower.packets_from_inside.clone();
        {
            packets_from_inside.lock().unwrap().push_back(buffer);
        }
        */
        //TODO: I think this is not necessary?
        //let (mutex, has_data_condition_variable) = &*lower.has_data.clone();
        //has_data_condition_variable.notify_one();
        //smoltcp::Result::Ok(_)
        r.unwrap()
    }
}
