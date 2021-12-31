use alloc::sync::Arc;
use libc::*;
use core::ops::Deref;
use super::super::super::qlib::mutex::*;

use super::super::super::IO_MGR;
use super::super::super::URING_MGR;
//use super::super::super::SHARE_SPACE;
use super::super::super::qlib::linux_def::*;
//use super::super::super::qlib::common::*;
//use super::super::super::qlib::task_mgr::*;
use super::super::super::qlib::socket_buf::*;
//use super::super::super::qlib::qmsg::qcall::*;
//use super::super::super::qlib::qmsg::input::*;
use super::fdinfo::*;
use super::socket_info::*;

#[derive(Clone)]
pub struct RDMAServerSock(Arc<QMutex<RDMAServerSockIntern>>);

impl Deref for RDMAServerSock {
    type Target = Arc<QMutex<RDMAServerSockIntern>>;

    fn deref(&self) -> &Arc<QMutex<RDMAServerSockIntern>> {
        &self.0
    }
}

impl RDMAServerSock {
    pub fn New(fd: i32, acceptQueue: AcceptQueue) -> Self {
        return Self (
            Arc::new(QMutex::new(RDMAServerSockIntern{
                fd: fd,
                acceptQueue: acceptQueue
            }))
        )
    }

    pub fn Notify(&self, _eventmask: EventMask) {
        self.Accept();
    }

    pub fn Accept(&self) {
        let minefd = self.lock().fd;
        let acceptQueue = self.lock().acceptQueue.clone();
        if acceptQueue.lock().Err() != 0 {
            FdNotify(minefd, EVENT_ERR | EVENT_IN);
            return
        }

        let mut hasSpace = acceptQueue.lock().HasSpace();

        while hasSpace {
            let tcpAddr = TcpSockAddr::default();
            let mut len : u32 = TCP_ADDR_LEN as _;

            let ret = unsafe{
                accept4(minefd, tcpAddr.Addr() as  *mut sockaddr, &mut len as  *mut socklen_t, SocketFlags::SOCK_NONBLOCK | SocketFlags::SOCK_CLOEXEC)
            };

            if ret < 0 {
                let errno = errno::errno().0;
                if errno == SysErr::EAGAIN {
                    return
                }

                FdNotify(minefd, EVENT_ERR | EVENT_IN);
                acceptQueue.lock().SetErr(errno);
                return
            }

            let fd = ret;

            IO_MGR.AddSocket(fd);
            let socketBuf = Arc::new(SocketBuff::default());

            let rdmaSocket = RDMADataSock::New(fd, socketBuf.clone());
            let fdInfo = IO_MGR.GetByHost(fd).unwrap();
            *fdInfo.lock().sockInfo.lock() = SockInfo::RDMADataSocket(rdmaSocket);

            URING_MGR.lock().Addfd(fd).unwrap();
            IO_MGR.AddWait(fd, EVENT_READ | EVENT_WRITE);

            let (trigger, tmp) = acceptQueue.lock().EnqSocket(fd, tcpAddr, len, socketBuf);
            hasSpace = tmp;

            if trigger {
                FdNotify(minefd, EVENT_IN);
            }
        }
    }
}

pub struct RDMAServerSockIntern {
    pub fd: i32,
    pub acceptQueue: AcceptQueue,
}

pub struct RDMADataSockIntern {
    pub fd: i32,
    pub socketBuf: Arc<SocketBuff>,
    pub readLock: QMutex<()>,
    pub writeLock: QMutex<()>
}

#[derive(Clone)]
pub struct RDMADataSock(Arc<RDMADataSockIntern>);

impl Deref for RDMADataSock {
    type Target = Arc<RDMADataSockIntern>;

    fn deref(&self) -> &Arc<RDMADataSockIntern> {
        &self.0
    }
}

impl RDMADataSock {
    pub fn New(fd: i32, socketBuf: Arc<SocketBuff>) -> Self {
        return Self (
            Arc::new(RDMADataSockIntern{
                fd: fd,
                socketBuf: socketBuf,
                readLock: QMutex::new(()),
                writeLock: QMutex::new(()),
            })
        )
    }

    pub fn Read(&self) {
        let _readlock = self.readLock.lock();

        let fd = self.fd;
        let socketBuf = self.socketBuf.clone();

        let (mut addr, mut count) = socketBuf.GetFreeReadBuf();
        if count == 0 { // no more space
            return
        }

        loop {
            let len = unsafe {
                read(fd, addr as _, count as _)
            };

            // closed
            if len == 0 {
                socketBuf.SetRClosed();
                if socketBuf.HasReadData() {
                    FdNotify(fd, EVENT_IN);
                } else {
                    FdNotify(fd, EVENT_HUP);
                }
                return
            }

            if len < 0 {
                let errno = errno::errno().0;
                if errno == SysErr::EAGAIN {
                    return
                }

                socketBuf.SetErr(errno);
                FdNotify(fd, EVENT_ERR | EVENT_IN);
                return
            }

            let (trigger, addrTmp, countTmp) = socketBuf.ProduceAndGetFreeReadBuf(len as _);
            if trigger {
                FdNotify(fd, EVENT_IN);
            }

            if len < count as _ { // have clean the read buffer
                return
            }

            if countTmp == 0 {  // no more space
                return
            }

            addr = addrTmp;
            count = countTmp;
        }
    }

    pub fn Write(&self) {
        let _writelock = self.writeLock.lock();

        let fd = self.fd;
        let socketBuf = self.socketBuf.clone();

        let (mut addr, mut count) = socketBuf.GetAvailableWriteBuf();
        if count == 0 { // no data
            return
        }

        loop {
            let len = unsafe {
                write(fd, addr as _, count as _)
            };

            // closed
            if len == 0 {
                socketBuf.SetWClosed();
                if socketBuf.HasWriteData() {
                    FdNotify(fd, EVENT_OUT);
                } else {
                    FdNotify(fd, EVENT_HUP);
                }
                return
            }

            if len < 0 {
                let errno = errno::errno().0;
                if errno == SysErr::EAGAIN {
                    return
                }

                socketBuf.SetErr(errno);
                FdNotify(fd, EVENT_ERR | EVENT_IN);
                return
            }

            let (trigger, addrTmp, countTmp) = socketBuf.ConsumeAndGetAvailableWriteBuf(len as _);
            if trigger {
                FdNotify(fd, EVENT_OUT);
            }

            if len < count as _ { // have fill the write buffer
                return
            }

            if countTmp == 0 {
                if socketBuf.PendingWriteShutdown() {
                    FdNotify(fd, EVENT_PENDING_SHUTDOWN);
                }

                return;
            }

            addr = addrTmp;
            count = countTmp;
        }
    }

    pub fn Notify(&self, eventmask: EventMask) {
        let fd = self.fd;
        let socketBuf = self.socketBuf.clone();

        if socketBuf.Error() != 0 {
            FdNotify(fd, EVENT_ERR | EVENT_IN);
            return
        }

        if eventmask & EVENT_READ != 0 {
            self.Read()
        } else if eventmask & EVENT_WRITE != 0 {
            self.Write()
        }
    }
}