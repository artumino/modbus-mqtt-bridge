//This code is based on embassy and tokio runners
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures::future::Either;

pub fn select<A, B>(a: A, b: B) -> Select<A, B>
where
    A: Future,
    B: Future,
{
    Select { a, b }
}

#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Select<A, B> {
    a: A,
    b: B,
}

impl<A: Unpin, B: Unpin> Unpin for Select<A, B> {}

impl<A, B> Future for Select<A, B>
where
    A: Future,
    B: Future,
{
    type Output = Either<A::Output, B::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let a = unsafe { Pin::new_unchecked(&mut this.a) };
        let b = unsafe { Pin::new_unchecked(&mut this.b) };
        if let Poll::Ready(x) = a.poll(cx) {
            return Poll::Ready(Either::Left(x));
        }
        if let Poll::Ready(x) = b.poll(cx) {
            return Poll::Ready(Either::Right(x));
        }
        Poll::Pending
    }
}