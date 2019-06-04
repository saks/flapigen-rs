use std::{io::Write, path::Path};

use log::debug;
use petgraph::Direction;
use proc_macro2::TokenStream;
use smol_str::SmolStr;
use syn::{parse_quote, spanned::Spanned, Type};

use crate::{
    cpp::{
        c_func_name, cpp_code, map_type::map_type, n_arguments_list, rust_generate_args_with_types,
        CppForeignMethodSignature, CppForeignTypeInfo, MethodContext,
    },
    error::{panic_on_syn_error, DiagnosticError, Result},
    file_cache::FileWriteCache,
    typemap::{
        ast::{fn_arg_type, list_lifetimes, normalize_ty_lifetimes, DisplayToTokens},
        ty::RustType,
        unpack_unique_typename,
        utils::{
            create_suitable_types_for_constructor_and_self,
            foreign_from_rust_convert_method_output, foreign_to_rust_convert_method_inputs,
        },
        ForeignTypeInfo, FROM_VAR_TEMPLATE, TO_VAR_TEMPLATE,
    },
    types::{ForeignerClassInfo, MethodAccess, MethodVariant, SelfTypeVariant},
    CppConfig, TypeMap,
};

pub(in crate::cpp) fn generate(
    conv_map: &mut TypeMap,
    output_dir: &Path,
    namespace_name: &str,
    target_pointer_width: usize,
    separate_impl_headers: bool,
    class: &ForeignerClassInfo,
    req_includes: &[SmolStr],
    methods_sign: &[CppForeignMethodSignature],
) -> Result<Vec<TokenStream>> {
    use std::fmt::Write;

    let c_path = output_dir.join(format!("c_{}.h", class.name));
    let mut c_include_f = FileWriteCache::new(&c_path);
    let cpp_path = output_dir.join(cpp_code::cpp_header_name(class));
    let mut cpp_include_f = FileWriteCache::new(&cpp_path);
    let cpp_fwd_path = output_dir.join(format!("{}_fwd.hpp", class.name));
    let mut cpp_fwd_f = FileWriteCache::new(&cpp_fwd_path);

    macro_rules! map_write_err {
        ($file_path:ident) => {
            |err| {
                DiagnosticError::new(
                    class.src_id,
                    class.span(),
                    format!("write to {} failed: {}", $file_path.display(), err),
                )
            }
        };
    }

    let c_class_type = cpp_code::c_class_type(class);
    let class_doc_comments = cpp_code::doc_comments_to_c_comments(&class.doc_comments, true);

    write!(
        c_include_f,
        r##"// Automaticaly generated by rust_swig
{doc_comments}
#pragma once

//for (u)intX_t types
#include <stdint.h>

#ifdef __cplusplus
static_assert(sizeof(uintptr_t) == sizeof(uint8_t) * {sizeof_usize},
   "our conversation usize <-> uintptr_t is wrong");
extern "C" {{
#endif

    typedef struct {c_class_type} {c_class_type};

"##,
        doc_comments = class_doc_comments,
        c_class_type = c_class_type,
        sizeof_usize = target_pointer_width / 8,
    )
    .map_err(map_write_err!(c_path))?;

    let class_name = format!("{}Wrapper", class.name);

    let mut includes = String::new();
    for inc in req_includes {
        writeln!(&mut includes, r#"#include {}"#, inc).unwrap();
    }

    write!(
        cpp_include_f,
        r#"// Automaticaly generated by rust_swig
#pragma once

//for std::abort
#include <cstdlib>
//for std::move
#include <utility>
//for std::conditional
#include <type_traits>

{includes}
#include "c_{class_dot_name}.h"

namespace {namespace} {{

template<bool>
class {class_name};
using {class_dot_name} = {class_name}<true>;
using {class_dot_name}Ref = {class_name}<false>;

{doc_comments}
template<bool OWN_DATA>
class {class_name} {{
public:
    using SelfType = typename std::conditional<OWN_DATA, {c_class_type} *, const {c_class_type} *>::type;
    using CForeignType = {c_class_type};
    using value_type = {class_name}<true>;
    friend class {class_name}<true>;
    friend class {class_name}<false>;

    {class_name}({class_name} &&o) noexcept: self_(o.self_)
    {{
        o.self_ = nullptr;
    }}
    {class_name} &operator=({class_name} &&o) noexcept
    {{
        assert(this != &o);
        free_mem(this->self_);
        self_ = o.self_;
        o.self_ = nullptr;
        return *this;
    }}
    explicit {class_name}(SelfType o) noexcept: self_(o) {{}}
    {c_class_type} *release() noexcept
    {{
        {c_class_type} *ret = self_;
        self_ = nullptr;
        return ret;
    }}
    explicit operator SelfType() const noexcept {{ return self_; }}
    {class_name}<false> as_rref() const noexcept {{ return {class_name}<false>{{ self_ }}; }}
    const {class_name}<true> &as_cref() const noexcept {{ return reinterpret_cast<const {class_name}<true> &>(*this); }}
"#,
        c_class_type = c_class_type,
        class_name = class_name,
        class_dot_name = class.name,
        includes = includes,
        doc_comments = class_doc_comments,
        namespace = namespace_name,
    ).map_err(map_write_err!(cpp_path))?;

    if !class.copy_derived {
        write!(
            cpp_include_f,
            r#"
    {class_name}(const {class_name}&) = delete;
    {class_name} &operator=(const {class_name}&) = delete;
"#,
            class_name = class_name
        )
        .map_err(map_write_err!(cpp_path))?;
    } else {
        let pos = class
            .methods
            .iter()
            .position(|m| {
                if let Some(seg) = m.rust_id.segments.last() {
                    let seg = seg.into_value();
                    seg.ident == "clone"
                } else {
                    false
                }
            })
            .ok_or_else(|| {
                DiagnosticError::new(
                    class.src_id,
                    class.span(),
                    format!(
                        "Class {} (namespace {}) has derived Copy attribute, but no clone method",
                        class.name, namespace_name,
                    ),
                )
            })?;
        let c_clone_func = c_func_name(class, &class.methods[pos]);

        write!(
            cpp_include_f,
            r#"
            {class_name}(const {class_name}& o) noexcept {{
                static_assert(OWN_DATA, "copy possible only if class own data");

                 if (o.self_ != nullptr) {{
                     self_ = {c_clone_func}(o.self_);
                 }} else {{
                     self_ = nullptr;
                 }}
            }}
            {class_name} &operator=(const {class_name}& o) noexcept {{
                static_assert(OWN_DATA, "copy possible only if class own data");
                if (this != &o) {{
                    free_mem(this->self_);
                    if (o.self_ != nullptr) {{
                        self_ = {c_clone_func}(o.self_);
                    }} else {{
                        self_ = nullptr;
                    }}
                }}
                return *this;
            }}
        "#,
            c_clone_func = c_clone_func,
            class_name = class_name
        )
        .map_err(map_write_err!(cpp_path))?;
    }

    let mut last_cpp_access = Some("public");

    let dummy_ty = parse_type! { () };
    let dummy_rust_ty = conv_map.find_or_alloc_rust_type_no_src_id(&dummy_ty);
    let mut gen_code = Vec::new();

    let (this_type_for_method, code_box_this) =
        if let Some(this_type) = class.constructor_ret_type.as_ref() {
            let this_type = conv_map.find_or_alloc_rust_type_that_implements(
                this_type,
                "SwigForeignClass",
                class.src_id,
            );

            let (this_type_for_method, code_box_this) =
                conv_map.convert_to_heap_pointer(&this_type, "this");
            let lifetimes = {
                let mut ret = String::new();
                let lifetimes = list_lifetimes(&this_type.ty);
                for (i, l) in lifetimes.iter().enumerate() {
                    ret.push_str(&*l.as_str());
                    if i != lifetimes.len() - 1 {
                        ret.push(',');
                    }
                }
                ret
            };
            let unpack_code = TypeMap::unpack_from_heap_pointer(&this_type, TO_VAR_TEMPLATE, true);
            let fclass_impl_code = format!(
                r#"impl<{lifetimes}> SwigForeignClass for {class_name} {{
    fn c_class_name() -> *const ::std::os::raw::c_char {{
        swig_c_str!("{class_name}")
    }}
    fn box_object(this: Self) -> *mut ::std::os::raw::c_void {{
{code_box_this}
        this as *mut ::std::os::raw::c_void
    }}
    fn unbox_object(p: *mut ::std::os::raw::c_void) -> Self {{
        let p = p as *mut {this_type_for_method};
{unpack_code}
       p
    }}
}}"#,
                lifetimes = lifetimes,
                class_name = DisplayToTokens(&this_type.ty),
                code_box_this = code_box_this,
                unpack_code = unpack_code.replace(TO_VAR_TEMPLATE, "p"),
                this_type_for_method = this_type_for_method.normalized_name.clone()
            );
            gen_code.push(syn::parse_str(&fclass_impl_code).unwrap_or_else(|err| {
                panic_on_syn_error("internal foreign class impl code", fclass_impl_code, err)
            }));
            (this_type_for_method, code_box_this)
        } else {
            (dummy_rust_ty.clone(), String::new())
        };
    let no_this_info = || {
        DiagnosticError::new(
            class.src_id,
            class.span(),
            format!(
                "Class {} (namespace {}) has methods, but there is no constructor\n
May be you need to use `private constructor = empty;` syntax?",
                class.name, namespace_name,
            ),
        )
    };

    let mut need_destructor = false;
    //because of VC++ has problem with cross-references of types
    let mut inline_impl = String::new();

    for (method, f_method) in class.methods.iter().zip(methods_sign) {
        write!(
            c_include_f,
            "{}",
            cpp_code::doc_comments_to_c_comments(&method.doc_comments, false)
        )
        .map_err(map_write_err!(c_path))?;

        let method_access = match method.access {
            MethodAccess::Private => "private",
            MethodAccess::Public => "public",
            MethodAccess::Protected => "protected",
        };
        if last_cpp_access
            .map(|last| last != method_access)
            .unwrap_or(true)
        {
            write!(cpp_include_f, "{}:\n", method_access).map_err(map_write_err!(cpp_path))?;
        }
        last_cpp_access = Some(method_access);
        let cpp_comments = cpp_code::doc_comments_to_c_comments(&method.doc_comments, false);
        write!(cpp_include_f, "{}", cpp_comments,).map_err(map_write_err!(cpp_path))?;
        let c_func_name = c_func_name(class, method);
        let c_args_with_types = cpp_code::c_generate_args_with_types(f_method, false)
            .map_err(|err| DiagnosticError::new(class.src_id, class.span(), err))?;
        let comma_c_args_with_types = if c_args_with_types.is_empty() {
            String::new()
        } else {
            format!(", {}", c_args_with_types)
        };
        let args_names = n_arguments_list(f_method.input.len());

        let cpp_args_with_types = cpp_code::cpp_generate_args_with_types(f_method)
            .map_err(|err| DiagnosticError::new(class.src_id, class.span(), err))?;
        let cpp_args_for_c = cpp_code::cpp_generate_args_to_call_c(f_method)
            .map_err(|err| DiagnosticError::new(class.src_id, class.span(), err))?;
        let real_output_typename = match method.fn_decl.output {
            syn::ReturnType::Default => "()",
            syn::ReturnType::Type(_, ref t) => normalize_ty_lifetimes(&*t),
        };

        let rust_args_with_types = rust_generate_args_with_types(f_method)
            .map_err(|err| DiagnosticError::new(class.src_id, class.span(), err))?;
        let method_ctx = MethodContext {
            class,
            method,
            f_method,
            c_func_name: &c_func_name,
            decl_func_args: &rust_args_with_types,
            args_names: &args_names,
            real_output_typename: &real_output_typename,
        };

        let method_name = method.short_name().as_str().to_string();
        let (cpp_ret_type, convert_ret_for_cpp) =
            if let Some(cpp_converter) = f_method.output.cpp_converter.as_ref() {
                (
                    cpp_converter.typename.clone(),
                    cpp_converter.converter.replace(FROM_VAR_TEMPLATE, "ret"),
                )
            } else {
                (f_method.output.as_ref().name.clone(), "ret".to_string())
            };
        //rename types like "struct Foo" to "Foo" to make VC++ compiler happy
        let cpp_ret_type = cpp_ret_type.as_str().replace("struct", "");

        match method.variant {
            MethodVariant::StaticMethod => {
                write!(
                    c_include_f,
                    r#"
    {ret_type} {c_func_name}({args_with_types});
"#,
                    ret_type = f_method.output.as_ref().name,
                    c_func_name = c_func_name,
                    args_with_types = c_args_with_types,
                )
                .map_err(map_write_err!(c_path))?;

                if f_method.output.as_ref().name != "void" {
                    write!(
                        cpp_include_f,
                        r#"
    static {cpp_ret_type} {method_name}({cpp_args_with_types}) noexcept;
"#,
                        method_name = method_name,
                        cpp_ret_type = cpp_ret_type,
                        cpp_args_with_types = cpp_args_with_types,
                    )
                    .map_err(map_write_err!(cpp_path))?;
                    write!(
                        &mut inline_impl,
                        r#"
    template<bool OWN_DATA>
    inline {cpp_ret_type} {class_name}<OWN_DATA>::{method_name}({cpp_args_with_types}) noexcept
    {{
        {c_ret_type} ret = {c_func_name}({cpp_args_for_c});
        return {convert_ret_for_cpp};
    }}
"#,
                        c_ret_type = f_method.output.as_ref().name,
                        convert_ret_for_cpp = convert_ret_for_cpp,
                        cpp_args_for_c = cpp_args_for_c,
                        c_func_name = c_func_name,
                        cpp_ret_type = cpp_ret_type,
                        class_name = class_name,
                        method_name = method_name,
                        cpp_args_with_types = cpp_args_with_types,
                    )
                    .unwrap();
                } else {
                    write!(
                        cpp_include_f,
                        r#"
    static void {method_name}({cpp_args_with_types}) noexcept;
"#,
                        method_name = method_name,
                        cpp_args_with_types = cpp_args_with_types,
                    )
                    .map_err(map_write_err!(cpp_path))?;
                    write!(
                        &mut inline_impl,
                        r#"
    template<bool OWN_DATA>
    inline void {class_name}<OWN_DATA>::{method_name}({cpp_args_with_types}) noexcept
    {{
        {c_func_name}({cpp_args_for_c});
    }}
"#,
                        cpp_args_with_types = cpp_args_with_types,
                        class_name = class_name,
                        method_name = method_name,
                        c_func_name = c_func_name,
                        cpp_args_for_c = cpp_args_for_c,
                    )
                    .unwrap();
                }
                gen_code.append(&mut generate_static_method(conv_map, &method_ctx)?);
            }
            MethodVariant::Method(ref self_variant) => {
                let const_if_readonly = if self_variant.is_read_only() {
                    "const "
                } else {
                    ""
                };
                write!(
                    c_include_f,
                    r#"
    {ret_type} {func_name}({const_if_readonly}{c_class_type} * const self{args_with_types});
"#,
                    ret_type = f_method.output.as_ref().name,
                    c_class_type = c_class_type,
                    func_name = c_func_name,
                    args_with_types = comma_c_args_with_types,
                    const_if_readonly = const_if_readonly,
                )
                .map_err(map_write_err!(c_path))?;

                if f_method.output.as_ref().name != "void" {
                    write!(
                        cpp_include_f,
                        r#"
    {cpp_ret_type} {method_name}({cpp_args_with_types}) {const_if_readonly} noexcept;
"#,
                        method_name = method_name,
                        cpp_ret_type = cpp_ret_type,
                        cpp_args_with_types = cpp_args_with_types,
                        const_if_readonly = const_if_readonly,
                    )
                    .map_err(map_write_err!(cpp_path))?;
                    write!(&mut inline_impl, r#"
    template<bool OWN_DATA>
    inline {cpp_ret_type} {class_name}<OWN_DATA>::{method_name}({cpp_args_with_types}) {const_if_readonly} noexcept
    {{
        {c_ret_type} ret = {c_func_name}(this->self_{cpp_args_for_c});
        return {convert_ret_for_cpp};
    }}
"#,
                           method_name = method_name,
                           convert_ret_for_cpp = convert_ret_for_cpp,
                           c_ret_type = f_method.output.as_ref().name,
                           class_name = class_name,
                           cpp_ret_type = cpp_ret_type,
                           c_func_name = c_func_name,
                           cpp_args_with_types = cpp_args_with_types,
                                                   cpp_args_for_c = if args_names.is_empty() {
                            String::new()
                        } else {
                            format!(", {}", cpp_args_for_c)
                                                   },
                           const_if_readonly = const_if_readonly,
                    ).unwrap();
                } else {
                    write!(
                        cpp_include_f,
                        r#"
    void {method_name}({cpp_args_with_types}) {const_if_readonly} noexcept;
"#,
                        method_name = method_name,
                        cpp_args_with_types = cpp_args_with_types,
                        const_if_readonly = const_if_readonly,
                    )
                    .map_err(map_write_err!(cpp_path))?;
                    write!(&mut inline_impl, r#"
    template<bool OWN_DATA>
    inline void {class_name}<OWN_DATA>::{method_name}({cpp_args_with_types}) {const_if_readonly} noexcept
    {{
        {c_func_name}(this->self_{cpp_args_for_c});
    }}
"#,
                           method_name = method_name,
                           c_func_name = c_func_name,
                           class_name = class_name,
                           cpp_args_with_types = cpp_args_with_types,
                           cpp_args_for_c = if args_names.is_empty() {
                               String::new()
                        } else {
                            format!(", {}", cpp_args_for_c)
                           },
                           const_if_readonly = const_if_readonly,
                    ).unwrap();
                }

                gen_code.append(&mut generate_method(
                    conv_map,
                    &method_ctx,
                    class,
                    *self_variant,
                    &this_type_for_method,
                )?);
            }
            MethodVariant::Constructor => {
                need_destructor = true;
                if method.is_dummy_constructor() {
                    write!(
                        cpp_include_f,
                        r#"
    {class_name}() noexcept {{}}
"#,
                        class_name = class_name,
                    )
                    .map_err(map_write_err!(cpp_path))?;
                } else {
                    write!(
                        c_include_f,
                        r#"
    {c_class_type} *{func_name}({args_with_types});
"#,
                        c_class_type = c_class_type,
                        func_name = c_func_name,
                        args_with_types = c_args_with_types,
                    )
                    .map_err(map_write_err!(c_path))?;

                    write!(
                        cpp_include_f,
                        r#"
    {class_name}({cpp_args_with_types}) noexcept
    {{
        this->self_ = {c_func_name}({cpp_args_for_c});
        if (this->self_ == nullptr) {{
            std::abort();
        }}
    }}
"#,
                        c_func_name = c_func_name,
                        cpp_args_with_types = cpp_args_with_types,
                        class_name = class_name,
                        cpp_args_for_c = cpp_args_for_c,
                    )
                    .map_err(map_write_err!(cpp_path))?;

                    let constructor_ret_type = class
                        .constructor_ret_type
                        .as_ref()
                        .ok_or_else(&no_this_info)?
                        .clone();
                    let this_type = constructor_ret_type.clone();
                    gen_code.append(&mut generate_constructor(
                        conv_map,
                        &method_ctx,
                        constructor_ret_type,
                        this_type,
                        &code_box_this,
                    )?);
                }
            }
        }
    }

    if need_destructor {
        let this_type: RustType = conv_map.find_or_alloc_rust_type(
            class
                .constructor_ret_type
                .as_ref()
                .ok_or_else(&no_this_info)?,
            class.src_id,
        );

        let unpack_code = TypeMap::unpack_from_heap_pointer(&this_type, "this", false);
        let c_destructor_name = format!("{}_delete", class.name);
        let code = format!(
            r#"
#[allow(unused_variables, unused_mut, non_snake_case)]
#[no_mangle]
pub extern "C" fn {c_destructor_name}(this: *mut {this_type}) {{
{unpack_code}
    drop(this);
}}
"#,
            c_destructor_name = c_destructor_name,
            unpack_code = unpack_code,
            this_type = this_type_for_method.normalized_name,
        );
        debug!("we generate and parse code: {}", code);
        gen_code.push(
            syn::parse_str(&code).unwrap_or_else(|err| {
                panic_on_syn_error("internal cpp desctructor code", code, err)
            }),
        );
        write!(
            c_include_f,
            r#"
    void {c_destructor_name}(const {c_class_type} *self);
"#,
            c_class_type = c_class_type,
            c_destructor_name = c_destructor_name,
        )
        .map_err(map_write_err!(c_path))?;

        write!(
            cpp_include_f,
            r#"
private:
   static void free_mem(SelfType &p) noexcept
   {{
        if (OWN_DATA && p != nullptr) {{
            {c_destructor_name}(p);
        }}
        p = nullptr;
   }}
public:
    ~{class_name}() noexcept
    {{
        free_mem(this->self_);
    }}
"#,
            c_destructor_name = c_destructor_name,
            class_name = class_name,
        )
        .map_err(map_write_err!(cpp_path))?;
    } else {
        // not need_destructor
        write!(
            cpp_include_f,
            r#"
private:
   static void free_mem(SelfType &) noexcept
   {{
   }}
"#,
        )
        .map_err(map_write_err!(cpp_path))?;
    }

    write!(
        c_include_f,
        r#"
#ifdef __cplusplus
}}
#endif

"#
    )
    .map_err(map_write_err!(c_path))?;

    write!(
        cpp_include_f,
        r#"
{foreigner_code}
private:
    SelfType self_;
}};
"#,
        foreigner_code = class.foreigner_code,
    )
    .map_err(map_write_err!(cpp_path))?;

    // Write method implementations.
    if separate_impl_headers {
        write!(
            cpp_include_f,
            r#"

}} // namespace {namespace}
"#,
            namespace = namespace_name
        )
        .map_err(map_write_err!(cpp_path))?;
        let cpp_impl_path = output_dir.join(format!("{}_impl.hpp", class.name));
        let mut cpp_impl_f = FileWriteCache::new(&cpp_impl_path);
        write!(
            cpp_impl_f,
            r#"// Automaticaly generated by rust_swig
#pragma once

#include "{class_name}.hpp"

namespace {namespace} {{
"#,
            class_name = class.name,
            namespace = namespace_name,
        )
        .map_err(map_write_err!(cpp_impl_path))?;
        write_methods_impls(&mut cpp_impl_f, namespace_name, &inline_impl)
            .map_err(map_write_err!(cpp_impl_path))?;
        cpp_impl_f
            .update_file_if_necessary()
            .map_err(map_write_err!(cpp_impl_path))?;
    } else {
        write_methods_impls(&mut cpp_include_f, namespace_name, &inline_impl)
            .map_err(map_write_err!(cpp_path))?;
    }

    write!(
        cpp_fwd_f,
        r#"// Automaticaly generated by rust_swig
#pragma once

namespace {namespace} {{
template<bool>
class {base_class_name};
using {class_name} = {base_class_name}<true>;
using {class_name}Ref = {base_class_name}<false>;
}} // namespace {namespace}
"#,
        namespace = namespace_name,
        class_name = class.name,
        base_class_name = class_name
    )
    .map_err(map_write_err!(cpp_fwd_path))?;

    cpp_fwd_f
        .update_file_if_necessary()
        .map_err(map_write_err!(cpp_fwd_path))?;
    c_include_f
        .update_file_if_necessary()
        .map_err(map_write_err!(c_path))?;
    cpp_include_f
        .update_file_if_necessary()
        .map_err(map_write_err!(cpp_path))?;
    Ok(gen_code)
}

fn generate_static_method(conv_map: &mut TypeMap, mc: &MethodContext) -> Result<Vec<TokenStream>> {
    let c_ret_type = unpack_unique_typename(
        &mc.f_method
            .output
            .as_ref()
            .correspoding_rust_type
            .normalized_name,
    );
    let (mut deps_code_out, convert_output_code) = foreign_from_rust_convert_method_output(
        conv_map,
        mc.class.src_id,
        &mc.method.fn_decl.output,
        mc.f_method.output.as_ref(),
        "ret",
        &c_ret_type,
    )?;
    let n_args = mc.f_method.input.len();
    let (deps_code_in, convert_input_code) = foreign_to_rust_convert_method_inputs(
        conv_map,
        mc.class.src_id,
        mc.method,
        mc.f_method,
        (0..n_args).map(|v| format!("a_{}", v)),
        &c_ret_type,
    )?;
    let code = format!(
        r#"
#[allow(non_snake_case, unused_variables, unused_mut)]
#[no_mangle]
pub extern "C" fn {func_name}({decl_func_args}) -> {c_ret_type} {{
{convert_input_code}
    let mut ret: {real_output_typename} = {rust_func_name}({args_names});
{convert_output_code}
    ret
}}
"#,
        func_name = mc.c_func_name,
        decl_func_args = mc.decl_func_args,
        c_ret_type = c_ret_type,
        convert_input_code = convert_input_code,
        rust_func_name = DisplayToTokens(&mc.method.rust_id),
        args_names = mc.args_names,
        convert_output_code = convert_output_code,
        real_output_typename = mc.real_output_typename,
    );
    let mut gen_code = deps_code_in;
    gen_code.append(&mut deps_code_out);
    gen_code.push(
        syn::parse_str(&code)
            .unwrap_or_else(|err| panic_on_syn_error("cpp internal static method", code, err)),
    );
    Ok(gen_code)
}

fn generate_method(
    conv_map: &mut TypeMap,
    mc: &MethodContext,
    class: &ForeignerClassInfo,
    self_variant: SelfTypeVariant,
    this_type_for_method: &RustType,
) -> Result<Vec<TokenStream>> {
    let c_ret_type = unpack_unique_typename(
        &mc.f_method
            .output
            .as_ref()
            .correspoding_rust_type
            .normalized_name,
    );
    let n_args = mc.f_method.input.len();
    let (deps_code_in, convert_input_code) = foreign_to_rust_convert_method_inputs(
        conv_map,
        mc.class.src_id,
        mc.method,
        mc.f_method,
        (0..n_args).map(|v| format!("a_{}", v)),
        &c_ret_type,
    )?;
    let (mut deps_code_out, convert_output_code) = foreign_from_rust_convert_method_output(
        conv_map,
        mc.class.src_id,
        &mc.method.fn_decl.output,
        mc.f_method.output.as_ref(),
        "ret",
        &c_ret_type,
    )?;
    //&mut constructor_real_type -> &mut class.self_type
    let (from_ty, to_ty): (Type, Type) = create_suitable_types_for_constructor_and_self(
        self_variant,
        class,
        &this_type_for_method.ty,
    );

    let from_ty = conv_map.find_or_alloc_rust_type(&from_ty, class.src_id);
    let to_ty = conv_map.find_or_alloc_rust_type(&to_ty, class.src_id);

    let (mut deps_this, convert_this) = conv_map.convert_rust_types(
        &from_ty,
        &to_ty,
        "this",
        &c_ret_type,
        (mc.class.src_id, mc.method.span()),
    )?;
    let code = format!(
        r#"
#[allow(non_snake_case, unused_variables, unused_mut)]
#[no_mangle]
pub extern "C" fn {func_name}(this: *mut {this_type}, {decl_func_args}) -> {c_ret_type} {{
{convert_input_code}
    let this: {this_type_ref} = unsafe {{
        this.as_mut().unwrap()
    }};
{convert_this}
    let mut ret: {real_output_typename} = {rust_func_name}(this, {args_names});
{convert_output_code}
    ret
}}
"#,
        func_name = mc.c_func_name,
        decl_func_args = mc.decl_func_args,
        convert_input_code = convert_input_code,
        c_ret_type = c_ret_type,
        this_type_ref = from_ty.normalized_name,
        this_type = this_type_for_method.normalized_name,
        convert_this = convert_this,
        rust_func_name = DisplayToTokens(&mc.method.rust_id),
        args_names = mc.args_names,
        convert_output_code = convert_output_code,
        real_output_typename = mc.real_output_typename,
    );

    let mut gen_code = deps_code_in;
    gen_code.append(&mut deps_code_out);
    gen_code.append(&mut deps_this);
    gen_code.push(
        syn::parse_str(&code)
            .unwrap_or_else(|err| panic_on_syn_error("cpp internal method", code, err)),
    );
    Ok(gen_code)
}

fn generate_constructor(
    conv_map: &mut TypeMap,
    mc: &MethodContext,
    construct_ret_type: Type,
    this_type: Type,
    code_box_this: &str,
) -> Result<Vec<TokenStream>> {
    let n_args = mc.f_method.input.len();
    let this_type: RustType = conv_map.ty_to_rust_type(&this_type);
    let ret_type_name = this_type.normalized_name.as_str();
    let (deps_code_in, convert_input_code) = foreign_to_rust_convert_method_inputs(
        conv_map,
        mc.class.src_id,
        mc.method,
        mc.f_method,
        (0..n_args).map(|v| format!("a_{}", v)),
        &ret_type_name,
    )?;
    let construct_ret_type: RustType = conv_map.ty_to_rust_type(&construct_ret_type);
    let (mut deps_this, convert_this) = conv_map.convert_rust_types(
        &construct_ret_type,
        &this_type,
        "this",
        &ret_type_name,
        (mc.class.src_id, mc.method.span()),
    )?;

    let code = format!(
        r#"
#[no_mangle]
#[allow(unused_variables, unused_mut, non_snake_case)]
pub extern "C" fn {func_name}({decl_func_args}) -> *const ::std::os::raw::c_void {{
{convert_input_code}
    let this: {real_output_typename} = {rust_func_name}({args_names});
{convert_this}
{box_this}
    this as *const ::std::os::raw::c_void
}}
"#,
        func_name = mc.c_func_name,
        convert_this = convert_this,
        decl_func_args = mc.decl_func_args,
        convert_input_code = convert_input_code,
        rust_func_name = DisplayToTokens(&mc.method.rust_id),
        args_names = mc.args_names,
        box_this = code_box_this,
        real_output_typename = &construct_ret_type.normalized_name.as_str(),
    );
    let mut gen_code = deps_code_in;
    gen_code.append(&mut deps_this);
    gen_code
        .push(syn::parse_str(&code).unwrap_or_else(|err| {
            panic_on_syn_error("cpp internal constructor method", code, err)
        }));
    Ok(gen_code)
}

fn write_methods_impls(
    file: &mut FileWriteCache,
    namespace_name: &str,
    inline_impl: &str,
) -> std::io::Result<()> {
    write!(
        file,
        r#"
{inline_impl}
}} // namespace {namespace}
"#,
        namespace = namespace_name,
        inline_impl = inline_impl,
    )
}

pub(in crate::cpp) fn find_suitable_foreign_types_for_methods(
    conv_map: &mut TypeMap,
    class: &ForeignerClassInfo,
    cpp_cfg: &CppConfig,
) -> Result<Vec<CppForeignMethodSignature>> {
    let mut ret = Vec::<CppForeignMethodSignature>::with_capacity(class.methods.len());
    let dummy_ty = parse_type! { () };
    let dummy_rust_ty = conv_map.find_or_alloc_rust_type_no_src_id(&dummy_ty);

    for method in &class.methods {
        //skip self argument
        let skip_n = match method.variant {
            MethodVariant::Method(_) => 1,
            _ => 0,
        };
        assert!(method.fn_decl.inputs.len() >= skip_n);
        let mut input =
            Vec::<CppForeignTypeInfo>::with_capacity(method.fn_decl.inputs.len() - skip_n);
        for arg in method.fn_decl.inputs.iter().skip(skip_n) {
            let arg_rust_ty = conv_map.find_or_alloc_rust_type(fn_arg_type(arg), class.src_id);
            input.push(map_type(
                conv_map,
                cpp_cfg,
                &arg_rust_ty,
                Direction::Incoming,
                (class.src_id, fn_arg_type(arg).span()),
            )?);
        }
        let output: CppForeignTypeInfo = match method.variant {
            MethodVariant::Constructor => ForeignTypeInfo {
                name: "".into(),
                correspoding_rust_type: dummy_rust_ty.clone(),
            }
            .into(),
            _ => match method.fn_decl.output {
                syn::ReturnType::Default => ForeignTypeInfo {
                    name: "void".into(),
                    correspoding_rust_type: dummy_rust_ty.clone(),
                }
                .into(),
                syn::ReturnType::Type(_, ref rt) => {
                    let ret_rust_ty = conv_map.find_or_alloc_rust_type(rt, class.src_id);
                    map_type(
                        conv_map,
                        cpp_cfg,
                        &ret_rust_ty,
                        Direction::Outgoing,
                        (class.src_id, rt.span()),
                    )?
                }
            },
        };
        ret.push(CppForeignMethodSignature { output, input });
    }
    Ok(ret)
}
