use super::{ProbeArgType, ProbeArgWrapper};
use std::fmt::Debug;

impl<'a, T> ProbeArgType<&'a Fn() -> T> for &'a Fn() -> T
where
    T: ProbeArgType<T> + Debug,
{
    type WrapperType = FuncProbeArgTypeWrapper<'a, T>;
}

pub struct FuncProbeArgTypeWrapper<'a, T>(&'a Fn() -> T)
where
    T: ProbeArgType<T> + Debug;

impl<'a, T> ProbeArgWrapper<&'a Fn() -> T> for FuncProbeArgTypeWrapper<'a, T>
where
    T: ProbeArgType<T> + Debug,
{
    type CType = <<T as ProbeArgType<T>>::WrapperType as ProbeArgWrapper<T>>::CType;

    fn new(arg: &'a Fn() -> T) -> Self {
        FuncProbeArgTypeWrapper(arg)
    }

    fn to_c_type(&mut self) -> Self::CType {
        let arg = self.0();
        let mut wrapped = super::wrap(arg);
        wrapped.to_c_type()
    }

    fn default_c_value() -> Self::CType {
        <<T as ProbeArgType<T>>::WrapperType as ProbeArgWrapper<T>>::default_c_value()
    }
}

impl<'a, T> Debug for FuncProbeArgTypeWrapper<'a, T>
where
    T: ProbeArgType<T> + Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        //Just use the Debug impl on the value returned by the function
        let arg = self.0();
        arg.fmt(f)
    }
}

//impl<CustomT, PrimitiveT> ProbeArgType<CustomT> for CustomT
//where
//    CustomT: CustomProbeArgType<PrimitiveT>,
//    PrimitiveT: ProbeArgType<PrimitiveT> + Sized + Debug,
//{
//    type WrapperType = CustomProbeArgWrapper<
//        CustomT,
//        PrimitiveT,
//        <PrimitiveT as ProbeArgType<PrimitiveT>>::WrapperType,
//    >;
//}
//
//pub struct CustomProbeArgWrapper<CustomT, PrimitiveT, PrimitiveWrapperT>
//where
//    CustomT: Into<PrimitiveT> + Sized,
//    PrimitiveWrapperT: ProbeArgWrapper<PrimitiveT>,
//    PrimitiveT: ProbeArgType<PrimitiveT> + Debug,
//{
//    arg: CustomT,
//    wrapper: Option<PrimitiveWrapperT>,
//}
//
//impl<CustomT, PrimitiveT, PrimitiveWrapperT> std::fmt::Debug
//    for CustomProbeArgWrapper<CustomT, PrimitiveT, PrimitiveWrapperT>
//where
//    CustomT: Into<PrimitiveT>,
//    PrimitiveWrapperT: ProbeArgWrapper<PrimitiveT>,
//    PrimitiveT: ProbeArgType<PrimitiveT> + Debug,
//{
//}
//
//impl<CustomT, PrimitiveT, PrimitiveWrapperT> ProbeArgWrapper<CustomT>
//    for CustomProbeArgWrapper<CustomT, PrimitiveT, PrimitiveWrapperT>
//where
//    CustomT: Into<PrimitiveT>,
//    PrimitiveWrapperT: ProbeArgWrapper<PrimitiveT>,
//    PrimitiveT: ProbeArgType<PrimitiveT> + Debug,
//{
//    type CType = PrimitiveWrapperT::CType;
//
//    fn to_c_type(&mut self) -> Self::CType {
//        let wrapped = super::wrap::<PrimitiveT>(self.0.into());
//        wrapped.to_c_type()
//    }
//    fn default_c_value() -> Self::CType {
//        PrimitiveWrapperT::default_c_value()
//    }
//}
