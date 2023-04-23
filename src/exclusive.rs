// 拷贝自源码

use core::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

#[derive(Default)]
pub struct Exclusive<T: ?Sized> {
    inner: T,
}

// See `Exclusive`'s docs for justification.
unsafe impl<T: ?Sized> Sync for Exclusive<T> {}

impl<T: ?Sized> fmt::Debug for Exclusive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("Exclusive").finish_non_exhaustive()
    }
}

impl<T: Sized> Exclusive<T> {
    /// Wrap a value in an `Exclusive`
    #[must_use]
    #[inline]
    pub fn new(t: T) -> Self {
        Self { inner: t }
    }

    /// Unwrap the value contained in the `Exclusive`
    #[must_use]
    #[inline]
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: ?Sized> Exclusive<T> {
    /// Get exclusive access to the underlying value.
    #[must_use]
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Get pinned exclusive access to the underlying value.
    ///
    /// `Exclusive` is considered to _structurally pin_ the underlying
    /// value, which means _unpinned_ `Exclusive`s can produce _unpinned_
    /// access to the underlying value, but _pinned_ `Exclusive`s only
    /// produce _pinned_ access to the underlying value.
    #[must_use]
    #[inline]
    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut T> {
        // SAFETY: `Exclusive` can only produce `&mut T` if itself is unpinned
        // `Pin::map_unchecked_mut` is not, so we do this conversion manually
        unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().inner) }
    }

    /// Build a _mutable_ reference to an `Exclusive<T>` from
    /// a _mutable_ reference to a `T`. This allows you to skip
    /// building an `Exclusive` with [`Exclusive::new`].
    #[must_use]
    #[inline]
    pub fn from_mut(r: &'_ mut T) -> &'_ mut Exclusive<T> {
        // SAFETY: repr is ≥ C, so refs have the same layout; and `Exclusive` properties are `&mut`-agnostic
        unsafe { &mut *(r as *mut T as *mut Exclusive<T>) }
    }

    /// Build a _pinned mutable_ reference to an `Exclusive<T>` from
    /// a _pinned mutable_ reference to a `T`. This allows you to skip
    /// building an `Exclusive` with [`Exclusive::new`].
    #[must_use]
    #[inline]
    pub fn from_pin_mut(r: Pin<&'_ mut T>) -> Pin<&'_ mut Exclusive<T>> {
        // SAFETY: `Exclusive` can only produce `&mut T` if itself is unpinned
        // `Pin::map_unchecked_mut` is not, so we do this conversion manually
        unsafe { Pin::new_unchecked(Self::from_mut(r.get_unchecked_mut())) }
    }
}

impl<T> From<T> for Exclusive<T> {
    #[inline]
    fn from(t: T) -> Self {
        Self::new(t)
    }
}

impl<T: Future + ?Sized> Future for Exclusive<T> {
    type Output = T::Output;
    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_pin_mut().poll(cx)
    }
}