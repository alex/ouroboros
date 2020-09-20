use inflector::Inflector;
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use proc_macro2::{Group, Span, TokenTree};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::{
    parenthesized, Attribute, Expr, Field, Fields, FieldsUnnamed, GenericParam, Generics, Ident,
    ItemStruct, Lifetime, LifetimeDef, Token, Type, TypeParam, TypeParamBound, Visibility,
};

#[derive(Clone, Copy, PartialEq)]
enum FieldType {
    /// Not borrowed by other parts of the struct.
    Tail,
    /// Immutably borrowed by at least one other field.
    Borrowed,
    /// Mutably borrowed by one other field.
    BorrowedMut,
}

impl FieldType {
    fn is_tail(self) -> bool {
        self == Self::Tail
    }
}

struct BorrowRequest {
    index: usize,
    mutable: bool,
}

struct StructFieldInfo {
    name: Ident,
    typ: Type,
    field_type: FieldType,
    borrows: Vec<BorrowRequest>,
}

impl StructFieldInfo {
    fn builder_name(&self) -> Ident {
        format_ident!("{}_builder", self.name)
    }

    fn illegal_ref_name(&self) -> Ident {
        format_ident!("{}_illegal_static_reference", self.name)
    }

    // Returns code which takes a variable with the same name and type as this field and turns it
    // into a static reference to its dereffed contents. For example, suppose a field
    // `test: Box<i32>`. This method would generate code that looks like:
    // ```rust
    // // Variable name taken from self.illegal_ref_name()
    // let test_illegal_static_reference = unsafe {
    //     ::ouroboros::macro_help::stable_deref_and_strip_lifetime(&test)
    // };
    // ```
    fn make_illegal_static_reference(&self) -> TokenStream2 {
        let field_name = &self.name;
        let ref_name = self.illegal_ref_name();
        quote! {
            let #ref_name = unsafe {
                ::ouroboros::macro_help::stable_deref_and_strip_lifetime(&#field_name)
            };
        }
    }

    /// Like make_illegal_static_reference, but provides a mutable reference instead.
    fn make_illegal_static_mut_reference(&self) -> TokenStream2 {
        let field_name = &self.name;
        let ref_name = self.illegal_ref_name();
        quote! {
            let #ref_name = unsafe {
                ::ouroboros::macro_help::stable_deref_and_strip_lifetime_mut(&mut #field_name)
            };
        }
    }
}

enum ArgType {
    /// Used when the initial value of a field can be passed directly into the constructor.
    Plain(TokenStream2),
    /// Used when a field requires self references and thus requires something that implements
    /// a builder function trait instead of a simple plain type.
    TraitBound(TokenStream2),
}

fn make_constructor_arg_type_impl(
    for_field: &StructFieldInfo,
    other_fields: &[StructFieldInfo],
    make_builder_return_type: impl FnOnce() -> TokenStream2,
) -> ArgType {
    let field_type = &for_field.typ;
    if for_field.borrows.len() == 0 {
        ArgType::Plain(quote! { #field_type })
    } else {
        let mut field_builder_params = Vec::new();
        for borrow in &for_field.borrows {
            if borrow.mutable {
                let field = &other_fields[borrow.index];
                let field_type = &field.typ;
                field_builder_params.push(quote! {
                    &'this mut <#field_type as ::std::ops::Deref>::Target
                });
            } else {
                let field = &other_fields[borrow.index];
                let field_type = &field.typ;
                field_builder_params.push(quote! {
                    &'this <#field_type as ::std::ops::Deref>::Target
                });
            }
        }
        let return_type = make_builder_return_type();
        let bound = quote! { for<'this> FnOnce(#(#field_builder_params),*) -> #return_type };
        ArgType::TraitBound(bound)
    }
}

/// Returns a trait bound if `for_field` refers to any other fields, and a plain type if not. This
/// is the type used in the constructor to initialize the value of `for_field`.
fn make_constructor_arg_type(
    for_field: &StructFieldInfo,
    other_fields: &[StructFieldInfo],
) -> ArgType {
    let field_type = &for_field.typ;
    make_constructor_arg_type_impl(for_field, other_fields, || quote! { #field_type })
}

/// Like make_constructor_arg_type, but used for the try_new constructor.
fn make_try_constructor_arg_type(
    for_field: &StructFieldInfo,
    other_fields: &[StructFieldInfo],
) -> ArgType {
    let field_type = &for_field.typ;
    make_constructor_arg_type_impl(
        for_field,
        other_fields,
        || quote! { Result<#field_type, Error_> },
    )
}

fn replace_this_with_static(input: TokenStream2) -> TokenStream2 {
    input
        .into_iter()
        .map(|token| match &token {
            TokenTree::Ident(ident) => {
                if ident.to_string() == "this" {
                    TokenTree::Ident(format_ident!("static"))
                } else {
                    token
                }
            }
            TokenTree::Group(group) => TokenTree::Group(Group::new(
                group.delimiter(),
                replace_this_with_static(group.stream()),
            )),
            _ => token,
        })
        .collect()
}

fn handle_borrows_attr(
    field_info: &mut [StructFieldInfo],
    attr: &Attribute,
    borrows: &mut Vec<BorrowRequest>,
) {
    let mut borrow_mut = false;
    let mut waiting_for_comma = false;
    let tokens = attr.tokens.clone();
    let tokens = if let Some(TokenTree::Group(group)) = tokens.into_iter().next() {
        group.stream()
    } else {
        panic!("Invalid syntax for borrows() macro.");
    };
    for token in tokens {
        if let TokenTree::Ident(ident) = token {
            if waiting_for_comma {
                panic!("Unexpected '{}', expected comma.", ident);
            }
            let istr = ident.to_string();
            if istr == "mut" {
                if borrow_mut {
                    panic!("Unexpected double 'mut' in borrows() macro.");
                }
                borrow_mut = true;
            } else {
                let index = field_info.iter().position(|item| item.name == istr);
                let index = if let Some(v) = index {
                    v
                } else {
                    panic!(
                        concat!(
                            "Unknown identifier '{}', make sure that it is spelled ",
                            "correctly and defined above the location it is borrowed."
                        ),
                        istr
                    );
                };
                if borrow_mut {
                    if field_info[index].field_type == FieldType::Borrowed {
                        panic!(
                            "Cannot borrow '{}' as mut as it was previously borrowed immutably.",
                            istr,
                        );
                    }
                    if field_info[index].field_type == FieldType::BorrowedMut {
                        panic!("Cannot borrow '{}' mutably more than once.", istr,)
                    }
                    field_info[index].field_type = FieldType::BorrowedMut;
                } else {
                    if field_info[index].field_type == FieldType::BorrowedMut {
                        panic!(
                            "Cannot borrow '{}' again as it was previously borrowed mutably.",
                            istr,
                        );
                    }
                    field_info[index].field_type = FieldType::Borrowed;
                }
                borrows.push(BorrowRequest {
                    index,
                    mutable: borrow_mut,
                });
                waiting_for_comma = true;
                borrow_mut = false;
            }
        } else if let TokenTree::Punct(punct) = token {
            if punct.as_char() == ',' {
                if waiting_for_comma {
                    waiting_for_comma = false;
                } else {
                    panic!("Unexpected extra comma in borrows() macro.");
                }
            } else {
                panic!(
                    "Unexpected punctuation {}, expected comma or identifier.",
                    punct
                );
            }
        } else {
            panic!("Unexpected token {}, expected comma or identifier.", token);
        }
    }
}

/// Creates the struct that will actually store the data. This involves properly organizing the
/// fields, collecting metadata about them, reversing the order everything is stored in, and
/// converting any uses of 'this to 'static.
fn create_actual_struct(original_struct_def: &ItemStruct) -> (TokenStream2, Vec<StructFieldInfo>) {
    let mut actual_struct_def = original_struct_def.clone();
    actual_struct_def.vis = syn::parse_quote! { pub };
    let mut field_info = Vec::new();
    match &mut actual_struct_def.fields {
        Fields::Named(fields) => {
            for field in &mut fields.named {
                let mut borrows = Vec::new();
                for (index, attr) in field.attrs.iter().enumerate() {
                    let path = &attr.path;
                    if path.leading_colon.is_some() {
                        continue;
                    }
                    if path.segments.len() != 1 {
                        continue;
                    }
                    if path.segments.first().unwrap().ident.to_string() == "borrows" {
                        handle_borrows_attr(&mut field_info[..], attr, &mut borrows);
                        field.attrs.remove(index);
                        break;
                    }
                }
                field.attrs.push(syn::parse_quote! { #[doc(hidden)] });
                field_info.push(StructFieldInfo {
                    name: field.ident.clone().expect("Named field has no name."),
                    typ: field.ty.clone(),
                    field_type: FieldType::Tail,
                    borrows,
                });
            }
        }
        Fields::Unnamed(_fields) => unimplemented!("Tuple structs are not supported yet."),
        Fields::Unit => panic!("Unit structs cannot be self-referential."),
    }
    if field_info.len() < 2 {
        panic!("Self-referencing structs must have at least 2 fields.");
    }
    let mut has_non_tail = false;
    for field in &field_info {
        if !field.field_type.is_tail() {
            has_non_tail = true;
            break;
        }
    }
    if !has_non_tail {
        panic!(
            concat!(
                "Self-referencing struct cannot be made entirely of tail fields, try adding ",
                "#[borrows({0})] to a field defined after {0}."
            ),
            field_info[0].name
        );
    }
    // Reverse the order of all fields. We ensure that items in the struct are only dependent
    // on references to items above them. Rust drops items in a struct in forward declaration order.
    // This would cause parents being dropped before children, necessitating the reversal.
    match &mut actual_struct_def.fields {
        Fields::Named(fields) => {
            let reversed = fields.named.iter().rev().cloned().collect();
            fields.named = reversed;
        }
        Fields::Unnamed(_fields) => unimplemented!("Tuple structs are not supported yet."),
        Fields::Unit => panic!("Unit structs cannot be self-referential."),
    }
    // Finally, replace the fake 'this lifetime with 'static.
    let actual_struct_def = replace_this_with_static(quote! { #actual_struct_def });

    (actual_struct_def, field_info)
}

// Takes the generics parameters from the original struct and turns them into arguments.
fn make_generic_arguments(generic_params: &Generics) -> Vec<TokenStream2> {
    let mut arguments = Vec::new();
    for generic in generic_params.params.clone() {
        match generic {
            GenericParam::Type(typ) => {
                let ident = &typ.ident;
                arguments.push(quote! { #ident });
            }
            GenericParam::Lifetime(lt) => {
                let lifetime = &lt.lifetime;
                arguments.push(quote! { #lifetime });
            }
            GenericParam::Const(_) => unimplemented!("Const generics are not supported yet."),
        }
    }
    arguments
}

fn create_builder_and_constructor(
    struct_name: &Ident,
    builder_struct_name: &Ident,
    generic_params: &Generics,
    generic_args: &Vec<TokenStream2>,
    field_info: &[StructFieldInfo],
) -> (TokenStream2, TokenStream2) {
    let documentation = format!(
        concat!(
            "Constructs a new instance of this self-referential struct. (See also ",
            "[`{0}::build()`]({0}::build)). Each argument is a field of ",
            "the new struct. Fields that refer to other fields inside the struct are initialized ",
            "using functions instead of directly passing their value. The arguments are as ",
            "follows:\n\n| Argument | Suggested Use |\n| --- | --- |\n",
        ),
        builder_struct_name.to_string()
    );
    let builder_documentation = concat!(
        "A more verbose but stable way to construct self-referencing structs. It is ",
        "comparable to using `StructName { field1: value1, field2: value2 }` rather than ",
        "`StructName::new(value1, value2)`. This has the dual benefit of making your code ",
        "both easier to refactor and more readable. Call [`build()`](Self::build) to ",
        "construct the actual struct. The fields of this struct should be used as follows:\n\n",
        "| Field | Suggested Use |\n| --- | --- |\n",
    )
    .to_owned();
    let build_fn_documentation = format!(
        concat!(
            "Calls [`{0}::new()`]({0}::new) using the provided values. This is preferrable over ",
            "calling `new()` directly for the reasons listed above. "
        ),
        struct_name.to_string()
    );
    let mut doc_table = "".to_owned();
    let mut code: Vec<TokenStream2> = Vec::new();
    let mut params: Vec<TokenStream2> = Vec::new();
    let mut builder_struct_generic_producers: Vec<_> = generic_params
        .params
        .iter()
        .map(|param| quote! { #param })
        .collect();
    let mut builder_struct_generic_consumers: Vec<_> = generic_args.clone();
    let mut builder_struct_fields = Vec::new();
    let mut builder_struct_field_names = Vec::new();

    for field in field_info {
        let field_name = &field.name;

        let arg_type = make_constructor_arg_type(&field, &field_info[..]);
        if let ArgType::Plain(plain_type) = arg_type {
            // No fancy builder function, we can just move the value directly into the struct.
            if field.field_type == FieldType::BorrowedMut {
                // If other fields borrow it mutably, we need to make the argument mutable.
                params.push(quote! { mut #field_name: #plain_type });
            } else {
                params.push(quote! { #field_name: #plain_type });
            }
            builder_struct_fields.push(quote! { #field_name: #plain_type });
            builder_struct_field_names.push(quote! { #field_name });
            doc_table += &format!(
                "| `{}` | Directly pass in the value this field should contain |\n",
                field_name.to_string()
            );
        } else if let ArgType::TraitBound(bound_type) = arg_type {
            // Trait bounds are much trickier. We need a special syntax to accept them in the
            // contructor, and generic parameters need to be added to the builder struct to make
            // it work.
            let builder_name = field.builder_name();
            params.push(quote! { #builder_name : impl #bound_type });
            // Ok so hear me out basically without this thing here my IDE thinks the rest of the
            // code is a string and it all turns green.
            {}
            doc_table += &format!(
                "| `{}` | Use a function or closure: `(",
                builder_name.to_string()
            );
            let mut builder_args = Vec::new();
            for (index, borrow) in field.borrows.iter().enumerate() {
                let borrowed_name = &field_info[borrow.index].name;
                builder_args.push(format_ident!("{}_illegal_static_reference", borrowed_name));
                doc_table += &format!(
                    "{}: &{}_",
                    borrowed_name.to_string(),
                    if borrow.mutable { "mut " } else { "" },
                );
                if index < field.borrows.len() - 1 {
                    doc_table += ", ";
                }
            }
            doc_table += &format!(") -> {}: _` | \n", field_name.to_string());
            if field.field_type == FieldType::BorrowedMut {
                // If other fields borrow it mutably, we need to make the variable mutable.
                code.push(quote! { let mut #field_name = #builder_name (#(#builder_args),*); });
            } else {
                code.push(quote! { let #field_name = #builder_name (#(#builder_args),*); });
            }
            let generic_type_name =
                format_ident!("{}Builder_", field_name.to_string().to_class_case());

            builder_struct_generic_producers.push(quote! { #generic_type_name: #bound_type });
            builder_struct_generic_consumers.push(quote! { #generic_type_name });
            builder_struct_fields.push(quote! { #builder_name: #generic_type_name });
            builder_struct_field_names.push(quote! { #builder_name });
        }

        if field.field_type == FieldType::Borrowed {
            code.push(field.make_illegal_static_reference());
        } else if field.field_type == FieldType::BorrowedMut {
            code.push(field.make_illegal_static_mut_reference());
        }
    }
    let field_names: Vec<_> = field_info.iter().map(|field| field.name.clone()).collect();
    let documentation = documentation + &doc_table;
    let builder_documentation = builder_documentation + &doc_table;
    let constructor_def = quote! {
        #[doc=#documentation]
        pub fn new(#(#params),*) -> Self {
            #(#code)*
            Self{ #(#field_names),* }
        }
    };
    let builder_def = quote! {
        #[doc=#builder_documentation]
        pub struct #builder_struct_name <#(#builder_struct_generic_producers),*> {
            #(pub #builder_struct_fields),*
        }
        impl<#(#builder_struct_generic_producers),*> #builder_struct_name <#(#builder_struct_generic_consumers),*> {
            #[doc=#build_fn_documentation]
            pub fn build(self) -> #struct_name <#(#generic_args),*> {
                #struct_name::new(
                    #(self.#builder_struct_field_names),*
                )
            }
        }
    };
    (builder_def, constructor_def)
}

fn create_try_builder_and_constructor(
    struct_name: &Ident,
    builder_struct_name: &Ident,
    generic_params: &Generics,
    generic_args: &Vec<TokenStream2>,
    field_info: &[StructFieldInfo],
) -> (TokenStream2, TokenStream2) {
    let mut head_field_names = Vec::new();
    for field in field_info {
        if field.borrows.len() == 0 {
            head_field_names.push(&field.name);
        }
    }

    let documentation = format!(
        concat!(
            "(See also [`{0}::try_build()`]({0}::try_build).) Like [`new`](Self::new), but ",
            "builders for [self-referencing fields](ouroboros::self_referencing) ",
            "can return results. If any of them fail, `Err` is returned. If all of them ",
            "succeed, `Ok` is returned. The arguments are as follows:\n\n",
            "| Argument | Suggested Use |\n| --- | --- |\n",
        ),
        builder_struct_name.to_string()
    );
    let or_recover_documentation = format!(
        concat!(
            "(See also [`{0}::try_build_or_recover()`]({0}::try_build_or_recover).) Like ",
            "[`try_new`](Self::try_new), but all ",
            "[head fields](ouroboros::self_referencing) ",
            "are returned in the case of an error. The arguments are as follows:\n\n",
            "| Argument | Suggested Use |\n| --- | --- |\n",
        ),
        builder_struct_name.to_string()
    );
    let builder_documentation = concat!(
        "A more verbose but stable way to construct self-referencing structs. It is ",
        "comparable to using `StructName { field1: value1, field2: value2 }` rather than ",
        "`StructName::new(value1, value2)`. This has the dual benefit of makin your code ",
        "both easier to refactor and more readable. Call [`try_build()`](Self::try_build) or ",
        "[`try_build_or_recover()`](Self::try_build_or_recover) to ",
        "construct the actual struct. The fields of this struct should be used as follows:\n\n",
        "| Field | Suggested Use |\n| --- | --- |\n",
    )
    .to_owned();
    let build_fn_documentation = format!(
        concat!(
            "Calls [`{0}::try_new()`]({0}::try_new) using the provided values. This is ",
            "preferrable over calling `try_new()` directly for the reasons listed above. "
        ),
        struct_name.to_string()
    );
    let build_or_recover_fn_documentation = format!(
        concat!(
            "Calls [`{0}::try_new_or_recover()`]({0}::try_new_or_recover) using the provided ",
            "values. This is preferrable over calling `try_new_or_recover()` directly for the ",
            "reasons listed above. "
        ),
        struct_name.to_string()
    );
    let mut doc_table = "".to_owned();
    let mut code: Vec<TokenStream2> = Vec::new();
    let mut or_recover_code: Vec<TokenStream2> = Vec::new();
    let mut params: Vec<TokenStream2> = Vec::new();
    let mut builder_struct_generic_producers: Vec<_> = generic_params
        .params
        .iter()
        .map(|param| quote! { #param })
        .collect();
    let mut builder_struct_generic_consumers: Vec<_> = generic_args.clone();
    let mut builder_struct_fields = Vec::new();
    let mut builder_struct_field_names = Vec::new();

    for field in field_info {
        let field_name = &field.name;

        let arg_type = make_try_constructor_arg_type(&field, &field_info[..]);
        if let ArgType::Plain(plain_type) = arg_type {
            // No fancy builder function, we can just move the value directly into the struct.
            if field.field_type == FieldType::BorrowedMut {
                // If other fields borrow it mutably, we need to make the argument mutable.
                params.push(quote! { mut #field_name: #plain_type });
            } else {
                params.push(quote! { #field_name: #plain_type });
            }
            builder_struct_fields.push(quote! { #field_name: #plain_type });
            builder_struct_field_names.push(quote! { #field_name });
            doc_table += &format!(
                "| `{}` | Directly pass in the value this field should contain |\n",
                field_name.to_string()
            );
        } else if let ArgType::TraitBound(bound_type) = arg_type {
            // Trait bounds are much trickier. We need a special syntax to accept them in the
            // contructor, and generic parameters need to be added to the builder struct to make
            // it work.
            let builder_name = field.builder_name();
            params.push(quote! { #builder_name : impl #bound_type });
            // Ok so hear me out basically without this thing here my IDE thinks the rest of the
            // code is a string and it all turns green.
            {}
            doc_table += &format!(
                "| `{}` | Use a function or closure: `(",
                builder_name.to_string()
            );
            let mut builder_args = Vec::new();
            for (index, borrow) in field.borrows.iter().enumerate() {
                let borrowed_name = &field_info[borrow.index].name;
                builder_args.push(format_ident!("{}_illegal_static_reference", borrowed_name));
                doc_table += &format!(
                    "{}: &{}_",
                    borrowed_name.to_string(),
                    if borrow.mutable { "mut " } else { "" },
                );
                if index < field.borrows.len() - 1 {
                    doc_table += ", ";
                }
            }
            doc_table += &format!(") -> Result<{}: _, Error_>` | \n", field_name.to_string());
            let maybe_mut = if field.field_type == FieldType::BorrowedMut {
                // If other fields borrow this field mutably, we need to make the variable mutable.
                quote! { mut }
            } else {
                quote! {}
            };
            code.push(quote! { let #maybe_mut #field_name = #builder_name (#(#builder_args),*)?; });
            or_recover_code.push(quote! {
                let #maybe_mut #field_name = match #builder_name (#(#builder_args),*) {
                    ::std::result::Result::Ok(value) => value,
                    ::std::result::Result::Err(err)
                        => return ::std::result::Result::Err((err, Heads { #(#head_field_names),* })),
                };
            });
            let generic_type_name =
                format_ident!("{}Builder_", field_name.to_string().to_class_case());

            builder_struct_generic_producers.push(quote! { #generic_type_name: #bound_type });
            builder_struct_generic_consumers.push(quote! { #generic_type_name });
            builder_struct_fields.push(quote! { #builder_name: #generic_type_name });
            builder_struct_field_names.push(quote! { #builder_name });
        }

        if field.field_type == FieldType::Borrowed {
            code.push(field.make_illegal_static_reference());
            or_recover_code.push(field.make_illegal_static_reference());
        } else if field.field_type == FieldType::BorrowedMut {
            code.push(field.make_illegal_static_mut_reference());
            or_recover_code.push(field.make_illegal_static_mut_reference());
        }
    }
    let field_names: Vec<_> = field_info.iter().map(|field| field.name.clone()).collect();
    let documentation = documentation + &doc_table;
    let or_recover_documentation = or_recover_documentation + &doc_table;
    let builder_documentation = builder_documentation + &doc_table;
    let constructor_def = quote! {
        #[doc=#documentation]
        pub fn try_new<Error_>(#(#params),*) -> ::std::result::Result<Self, Error_> {
            #(#code)*
            ::std::result::Result::Ok(Self{ #(#field_names),* })
        }
        #[doc=#or_recover_documentation]
        pub fn try_new_or_recover<Error_>(#(#params),*) -> ::std::result::Result<Self, (Error_, Heads<#(#generic_args),*>)> {
            #(#or_recover_code)*
            ::std::result::Result::Ok(Self{ #(#field_names),* })
        }
    };
    builder_struct_generic_producers.push(quote! { Error_ });
    builder_struct_generic_consumers.push(quote! { Error_ });
    let builder_def = quote! {
        #[doc=#builder_documentation]
        pub struct #builder_struct_name <#(#builder_struct_generic_producers),*> {
            #(pub #builder_struct_fields),*
        }
        impl<#(#builder_struct_generic_producers),*> #builder_struct_name <#(#builder_struct_generic_consumers),*> {
            #[doc=#build_fn_documentation]
            pub fn try_build(self) -> Result<#struct_name <#(#generic_args),*>, Error_> {
                #struct_name::try_new(
                    #(self.#builder_struct_field_names),*
                )
            }
            #[doc=#build_or_recover_fn_documentation]
            pub fn try_build_or_recover(self) -> Result<#struct_name <#(#generic_args),*>, (Error_, Heads<#(#generic_args),*>)> {
                #struct_name::try_new_or_recover(
                    #(self.#builder_struct_field_names),*
                )
            }
        }
    };
    (builder_def, constructor_def)
}

fn make_use_functions(field_info: &[StructFieldInfo]) -> Vec<TokenStream2> {
    let mut users = Vec::new();
    for field in field_info {
        let field_name = &field.name;
        let field_type = &field.typ;
        // If the field is not a tail, we need to serve up the same kind of reference that other
        // fields in the struct may have borrowed to ensure safety.
        if field.field_type == FieldType::Tail {
            let user_name = format_ident!("use_{}", &field.name);
            let documentation = format!(
                concat!(
                    "Provides an immutable reference to `{0}`. This method was generated because ",
                    "`{0}` is a [tail field](ouroboros::self_referencing)."
                ),
                field.name.to_string()
            );
            users.push(quote! {
                #[doc=#documentation]
                pub fn #user_name <'outer_borrow, ReturnType>(
                    &'outer_borrow self,
                    user: impl for<'this> FnOnce(&'outer_borrow #field_type) -> ReturnType,
                ) -> ReturnType {
                    user(&self. #field_name)
                }
            });
            // If it is not borrowed at all it's safe to allow mutably borrowing it.
            let user_name = format_ident!("use_{}_mut", &field.name);
            let documentation = format!(
                concat!(
                    "Provides a mutable reference to `{0}`. This method was generated because ",
                    "`{0}` is a [tail field](ouroboros::self_referencing)."
                ),
                field.name.to_string()
            );
            users.push(quote! {
                #[doc=#documentation]
                pub fn #user_name <'outer_borrow, ReturnType>(
                    &'outer_borrow mut self,
                    user: impl for<'this> FnOnce(&'outer_borrow mut #field_type) -> ReturnType,
                ) -> ReturnType {
                    user(&mut self. #field_name)
                }
            });
        } else if field.field_type == FieldType::Borrowed {
            let user_name = format_ident!("use_{}_contents", &field.name);
            let documentation = format!(
                concat!(
                    "Provides limited immutable access to the contents of `{0}`. This method was ",
                    "generated because `{0}` is immutably borrowed by other fields."
                ),
                field.name.to_string()
            );
            users.push(quote! {
                #[doc=#documentation]
                pub fn #user_name <'outer_borrow, ReturnType>(
                    &'outer_borrow self,
                    user: impl for<'this> FnOnce(&'outer_borrow <#field_type as ::std::ops::Deref>::Target) -> ReturnType,
                ) -> ReturnType {
                    user(&*self. #field_name)
                }
            });
        } else if field.field_type == FieldType::BorrowedMut {
            // Do not generate anything becaue if it is borrowed mutably once, we should not be able
            // to get any other kinds of references to it.
        }
    }
    users
}

fn make_use_all_function(
    struct_name: &Ident,
    field_info: &[StructFieldInfo],
    generic_params: &Generics,
    generic_args: &Vec<TokenStream2>,
) -> (TokenStream2, TokenStream2) {
    let mut fields = Vec::new();
    let mut field_assignments = Vec::new();
    let mut mut_fields = Vec::new();
    let mut mut_field_assignments = Vec::new();
    // I don't think the reverse is necessary but it does make the expanded code more uniform.
    for field in field_info.iter().rev() {
        let field_name = &field.name;
        let field_type = &field.typ;
        if field.field_type == FieldType::Tail {
            fields.push(quote! { pub #field_name: &'outer_borrow #field_type });
            field_assignments.push(quote! { #field_name: &self.#field_name });
            mut_fields.push(quote! { pub #field_name: &'outer_borrow mut #field_type });
            mut_field_assignments.push(quote! { #field_name: &mut self.#field_name });
        } else if field.field_type == FieldType::Borrowed {
            let value_name = format_ident!("{}_contents", field_name);
            fields.push(quote! { pub #value_name: &'outer_borrow <#field_type as ::std::ops::Deref>::Target });
            field_assignments.push(quote! { #value_name: &*self.#field_name });
        } else if field.field_type == FieldType::BorrowedMut {
            // Add nothing because we cannot borrow something that has already been mutably
            // borrowed.
        }
    }

    let new_generic_params = if generic_params.params.len() == 0 {
        quote! { <'outer_borrow, 'this> }
    } else {
        let mut new_generic_params = generic_params.clone();
        new_generic_params
            .params
            .insert(0, syn::parse_quote! { 'this });
        new_generic_params
            .params
            .insert(0, syn::parse_quote! { 'outer_borrow });
        quote! { #new_generic_params }
    };
    let new_generic_args = {
        let mut args = generic_args.clone();
        args.insert(0, quote! { 'this });
        args.insert(0, quote! { 'outer_borrow });
        args
    };

    let struct_documentation = format!(
        concat!(
            "A struct for holding immutable references to all ",
            "[tail and immutably borrowed fields](ouroboros::self_referencing) in an instance of ",
            "[`{0}`]({0})."
        ),
        struct_name.to_string()
    );
    let mut_struct_documentation = format!(
        concat!(
            "A struct for holding mutable references to all ",
            "[tail fields](ouroboros::self_referencing) in an instance of ",
            "[`{0}`]({0})."
        ),
        struct_name.to_string()
    );
    let struct_defs = quote! {
        #[doc=#struct_documentation]
        pub struct BorrowedFields #new_generic_params { #(#fields),* }
        #[doc=#mut_struct_documentation]
        pub struct BorrowedMutFields #new_generic_params { #(#mut_fields),* }
    };
    let borrowed_fields_type = quote! { BorrowedFields<#(#new_generic_args),*> };
    let borrowed_mut_fields_type = quote! { BorrowedMutFields<#(#new_generic_args),*> };
    let documentation = concat!(
        "This method provides immutable references to all ",
        "[tail and immutably borrowed fields](ouroboros::self_referencing).",
    );
    let mut_documentation = concat!(
        "This method provides mutable references to all ",
        "[tail fields](ouroboros::self_referencing).",
    );
    let fn_defs = quote! {
        #[doc=#documentation]
        pub fn use_all_fields <'outer_borrow, ReturnType>(
            &'outer_borrow self,
            user: impl for <'this> FnOnce(#borrowed_fields_type) -> ReturnType
        ) -> ReturnType {
            user(BorrowedFields {
                #(#field_assignments),*
            })
        }
        #[doc=#mut_documentation]
        pub fn use_all_fields_mut <'outer_borrow, ReturnType>(
            &'outer_borrow mut self,
            user: impl for <'this> FnOnce(#borrowed_mut_fields_type) -> ReturnType
        ) -> ReturnType {
            user(BorrowedMutFields {
                #(#mut_field_assignments),*
            })
        }
    };
    (struct_defs, fn_defs)
}

/// Returns the Heads struct and a function to convert the original struct into a Heads instance.
fn make_into_heads(
    struct_name: &Ident,
    field_info: &[StructFieldInfo],
    generic_params: &Generics,
    generic_args: &Vec<TokenStream2>,
) -> (TokenStream2, TokenStream2) {
    let mut code = Vec::new();
    let mut field_names = Vec::new();
    let mut head_fields = Vec::new();
    // Drop everything in the reverse order of what it was declared in. Fields that come later
    // are only dependent on fields that came before them.
    for field in field_info.iter().rev() {
        let field_name = &field.name;
        // Heads are fields that do not borrow anything.
        if field.borrows.len() > 0 {
            code.push(quote! { drop(self.#field_name); });
        } else {
            code.push(quote! { let #field_name = self.#field_name; });
            field_names.push(field_name);
            let field_type = &field.typ;
            head_fields.push(quote! { pub #field_name: #field_type });
        }
    }
    let documentation = format!(
        concat!(
            "A struct which contains only the ",
            "[head fields](ouroboros::self_referencing) of [`{0}`]({0})."
        ),
        struct_name.to_string()
    );
    let heads_struct_def = quote! {
        #[doc=#documentation]
        pub struct Heads #generic_params {
            #(#head_fields),*
        }
    };
    let into_heads_fn = quote! {
        pub fn into_heads(self) -> Heads<#(#generic_args),*> {
            #(#code)*
            Heads {
                #(#field_names),*
            }
        }
    };
    (heads_struct_def, into_heads_fn)
}

#[proc_macro_attribute]
pub fn self_referencing(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let original_struct_def: ItemStruct = syn::parse_macro_input!(item);
    let struct_name = &original_struct_def.ident;
    let mod_name = format_ident!("ouroboros_impl_{}", struct_name.to_string().to_snake_case());
    let visibility = &original_struct_def.vis;

    let (actual_struct_def, field_info) = create_actual_struct(&original_struct_def);

    let generic_params = original_struct_def.generics.clone();
    let generic_args = make_generic_arguments(&generic_params);

    let builder_struct_name = format_ident!("{}Builder", struct_name);
    let (builder_def, constructor_def) = create_builder_and_constructor(
        &struct_name,
        &builder_struct_name,
        &generic_params,
        &generic_args,
        &field_info[..],
    );
    let try_builder_struct_name = format_ident!("{}TryBuilder", struct_name);
    let (try_builder_def, try_constructor_def) = create_try_builder_and_constructor(
        &struct_name,
        &try_builder_struct_name,
        &generic_params,
        &generic_args,
        &field_info[..],
    );

    let users = make_use_functions(&field_info[..]);
    let (use_all_struct_defs, use_all_fn_defs) =
        make_use_all_function(struct_name, &field_info[..], &generic_params, &generic_args);
    let (heads_struct_def, into_heads_fn) =
        make_into_heads(struct_name, &field_info[..], &generic_params, &generic_args);

    TokenStream::from(quote! {
        mod #mod_name {
            #actual_struct_def
            #builder_def
            #try_builder_def
            #use_all_struct_defs
            #heads_struct_def
            impl #generic_params #struct_name <#(#generic_args),*> {
                #constructor_def
                #try_constructor_def
                #(#users)*
                #use_all_fn_defs
                #into_heads_fn
            }
        }
        #visibility use #mod_name :: #struct_name;
        #visibility use #mod_name :: #builder_struct_name;
        #visibility use #mod_name :: #try_builder_struct_name;
    })
}