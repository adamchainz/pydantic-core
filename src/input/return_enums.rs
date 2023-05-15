use std::borrow::Cow;
use std::slice::Iter as SliceIter;

use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::iter::PyDictIterator;
use pyo3::types::{PyBytes, PyDict, PyIterator, PyList, PyMapping, PyString, PyTuple};

#[cfg(not(PyPy))]
use pyo3::types::PyFunction;
#[cfg(not(PyPy))]
use pyo3::PyTypeInfo;

use crate::errors::{py_err_string, ErrorType, InputValue, ValError, ValResult};

use super::parse_json::{JsonArray, JsonInput, JsonObject};
use super::Input;

macro_rules! derive_from {
    ($enum:ident, $key:ident, $type:ty $(, $extra_types:ident )*) => {
        impl<'a> From<&'a $type> for $enum<'a> {
            fn from(s: &'a $type) -> $enum<'a> {
                Self::$key(s $(, $extra_types )*)
            }
        }
    };
}

#[cfg_attr(debug_assertions, derive(Debug))]
pub enum GenericMapping<'a> {
    PyDict(&'a PyDict),
    PyMapping(&'a PyMapping),
    PyGetAttr(&'a PyAny, Option<&'a PyDict>),
    JsonObject(&'a JsonObject),
}

derive_from!(GenericMapping, PyDict, PyDict);
derive_from!(GenericMapping, PyMapping, PyMapping);
derive_from!(GenericMapping, PyGetAttr, PyAny, None);
derive_from!(GenericMapping, JsonObject, JsonObject);

pub struct DictGenericIterator<'py> {
    dict_iter: PyDictIterator<'py>,
}

impl<'py> DictGenericIterator<'py> {
    pub fn new(dict: &'py PyDict) -> ValResult<'py, Self> {
        Ok(Self { dict_iter: dict.iter() })
    }
}

impl<'py> Iterator for DictGenericIterator<'py> {
    type Item = ValResult<'py, (&'py PyAny, &'py PyAny)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.dict_iter.next().map(Ok)
    }
    // size_hint is omitted as it isn't needed
}

pub struct MappingGenericIterator<'py> {
    input: &'py PyAny,
    iter: &'py PyIterator,
}

fn mapping_err<'py>(err: PyErr, py: Python<'py>, input: &'py impl Input<'py>) -> ValError<'py> {
    ValError::new(
        ErrorType::MappingType {
            error: py_err_string(py, err).into(),
        },
        input,
    )
}

impl<'py> MappingGenericIterator<'py> {
    pub fn new(mapping: &'py PyMapping) -> ValResult<'py, Self> {
        let py = mapping.py();
        let input: &PyAny = mapping;
        let iter = mapping
            .items()
            .map_err(|e| mapping_err(e, py, input))?
            .iter()
            .map_err(|e| mapping_err(e, py, input))?;
        Ok(Self { iter, input })
    }
}

const MAPPING_TUPLE_ERROR: &str = "Mapping items must be tuples of (key, value) pairs";

impl<'py> Iterator for MappingGenericIterator<'py> {
    type Item = ValResult<'py, (&'py PyAny, &'py PyAny)>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = match self.iter.next() {
            Some(Err(e)) => return Some(Err(mapping_err(e, self.iter.py(), self.input))),
            Some(Ok(item)) => item,
            None => return None,
        };
        let tuple: &PyTuple = match item.downcast() {
            Ok(tuple) => tuple,
            Err(_) => {
                return Some(Err(ValError::new(
                    ErrorType::MappingType {
                        error: MAPPING_TUPLE_ERROR.into(),
                    },
                    self.input,
                )))
            }
        };
        if tuple.len() != 2 {
            return Some(Err(ValError::new(
                ErrorType::MappingType {
                    error: MAPPING_TUPLE_ERROR.into(),
                },
                self.input,
            )));
        };
        #[cfg(PyPy)]
        let key = tuple.get_item(0).unwrap();
        #[cfg(PyPy)]
        let value = tuple.get_item(1).unwrap();
        #[cfg(not(PyPy))]
        let key = unsafe { tuple.get_item_unchecked(0) };
        #[cfg(not(PyPy))]
        let value = unsafe { tuple.get_item_unchecked(1) };
        Some(Ok((key, value)))
    }
    // size_hint is omitted as it isn't needed
}

pub struct AttributesGenericIterator<'py> {
    object: &'py PyAny,
    attributes: &'py PyList,
    index: usize,
}

impl<'py> AttributesGenericIterator<'py> {
    pub fn new(py_any: &'py PyAny) -> ValResult<'py, Self> {
        Ok(Self {
            object: py_any,
            attributes: py_any.dir(),
            index: 0,
        })
    }
}

impl<'py> Iterator for AttributesGenericIterator<'py> {
    type Item = ValResult<'py, (&'py PyAny, &'py PyAny)>;

    fn next(&mut self) -> Option<Self::Item> {
        // loop until we find an attribute who's name does not start with underscore,
        // or we get to the end of the list of attributes
        while self.index < self.attributes.len() {
            #[cfg(PyPy)]
            let name: &PyAny = self.attributes.get_item(self.index).unwrap();
            #[cfg(not(PyPy))]
            let name: &PyAny = unsafe { self.attributes.get_item_unchecked(self.index) };
            self.index += 1;
            // from benchmarks this is 14x faster than using the python `startswith` method
            let name_cow = match name.downcast::<PyString>() {
                Ok(name) => name.to_string_lossy(),
                Err(e) => return Some(Err(e.into())),
            };
            if !name_cow.as_ref().starts_with('_') {
                // getattr is most likely to fail due to an exception in a @property, skip
                if let Ok(attr) = self.object.getattr(name_cow.as_ref()) {
                    // we don't want bound methods to be included, is there a better way to check?
                    // ref https://stackoverflow.com/a/18955425/949890
                    let is_bound = matches!(attr.hasattr(intern!(attr.py(), "__self__")), Ok(true));
                    // the PyFunction::is_type_of(attr) catches `staticmethod`, but also any other function,
                    // I think that's better than including static methods in the yielded attributes,
                    // if someone really wants fields, they can use an explicit field, or a function to modify input
                    #[cfg(not(PyPy))]
                    if !is_bound && !PyFunction::is_type_of(attr) {
                        return Some(Ok((name, attr)));
                    }
                    // MASSIVE HACK! PyFunction doesn't exist for PyPy,
                    // is_instance_of::<PyFunction> crashes with a null pointer, hence this hack, see
                    // https://github.com/pydantic/pydantic-core/pull/161#discussion_r917257635
                    #[cfg(PyPy)]
                    if !is_bound && attr.get_type().to_string() != "<class 'function'>" {
                        return Some(Ok((name, attr)));
                    }
                }
            }
        }
        None
    }
    // size_hint is omitted as it isn't needed
}

pub struct JsonObjectGenericIterator<'py> {
    object_iter: SliceIter<'py, (String, JsonInput)>,
}

impl<'py> JsonObjectGenericIterator<'py> {
    pub fn new(json_object: &'py JsonObject) -> ValResult<'py, Self> {
        Ok(Self {
            object_iter: json_object.iter(),
        })
    }
}

impl<'py> Iterator for JsonObjectGenericIterator<'py> {
    type Item = ValResult<'py, (&'py String, &'py JsonInput)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.object_iter.next().map(|(key, value)| Ok((key, value)))
    }
    // size_hint is omitted as it isn't needed
}

#[derive(Debug, Clone)]
pub enum GenericIterator {
    PyIterator(GenericPyIterator),
    JsonArray(GenericJsonIterator),
}

impl From<JsonArray> for GenericIterator {
    fn from(array: JsonArray) -> Self {
        let length = array.len();
        let json_iter = GenericJsonIterator {
            array,
            length,
            index: 0,
        };
        Self::JsonArray(json_iter)
    }
}

impl From<&PyAny> for GenericIterator {
    fn from(obj: &PyAny) -> Self {
        let py_iter = GenericPyIterator {
            obj: obj.to_object(obj.py()),
            iter: obj.iter().unwrap().into_py(obj.py()),
            index: 0,
        };
        Self::PyIterator(py_iter)
    }
}

#[derive(Debug, Clone)]
pub struct GenericPyIterator {
    obj: PyObject,
    iter: Py<PyIterator>,
    index: usize,
}

impl GenericPyIterator {
    pub fn next<'a>(&'a mut self, py: Python<'a>) -> PyResult<Option<(&'a PyAny, usize)>> {
        match self.iter.as_ref(py).next() {
            Some(Ok(next)) => {
                let a = (next, self.index);
                self.index += 1;
                Ok(Some(a))
            }
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }

    pub fn input<'a>(&'a self, py: Python<'a>) -> &'a PyAny {
        self.obj.as_ref(py)
    }

    pub fn index(&self) -> usize {
        self.index
    }
}

#[derive(Debug, Clone)]
pub struct GenericJsonIterator {
    array: JsonArray,
    length: usize,
    index: usize,
}

impl GenericJsonIterator {
    pub fn next(&mut self, _py: Python) -> PyResult<Option<(&JsonInput, usize)>> {
        if self.index < self.length {
            let next = unsafe { self.array.get_unchecked(self.index) };
            let a = (next, self.index);
            self.index += 1;
            Ok(Some(a))
        } else {
            Ok(None)
        }
    }

    pub fn input<'a>(&'a self, py: Python<'a>) -> &'a PyAny {
        let input = JsonInput::Array(self.array.clone());
        input.to_object(py).into_ref(py)
    }

    pub fn index(&self) -> usize {
        self.index
    }
}

#[cfg_attr(debug_assertions, derive(Debug))]
pub struct PyArgs<'a> {
    pub args: Option<&'a PyTuple>,
    pub kwargs: Option<&'a PyDict>,
}

impl<'a> PyArgs<'a> {
    pub fn new(args: Option<&'a PyTuple>, kwargs: Option<&'a PyDict>) -> Self {
        Self { args, kwargs }
    }
}

#[cfg_attr(debug_assertions, derive(Debug))]
pub struct JsonArgs<'a> {
    pub args: Option<&'a [JsonInput]>,
    pub kwargs: Option<&'a JsonObject>,
}

impl<'a> JsonArgs<'a> {
    pub fn new(args: Option<&'a [JsonInput]>, kwargs: Option<&'a JsonObject>) -> Self {
        Self { args, kwargs }
    }
}

#[cfg_attr(debug_assertions, derive(Debug))]
pub enum GenericArguments<'a> {
    Py(PyArgs<'a>),
    Json(JsonArgs<'a>),
}

impl<'a> From<PyArgs<'a>> for GenericArguments<'a> {
    fn from(s: PyArgs<'a>) -> GenericArguments<'a> {
        Self::Py(s)
    }
}

impl<'a> From<JsonArgs<'a>> for GenericArguments<'a> {
    fn from(s: JsonArgs<'a>) -> GenericArguments<'a> {
        Self::Json(s)
    }
}

#[cfg_attr(debug_assertions, derive(Debug))]
pub enum EitherString<'a> {
    Cow(Cow<'a, str>),
    Py(&'a PyString),
}

impl<'a> EitherString<'a> {
    pub fn as_cow(&self) -> ValResult<'a, Cow<str>> {
        match self {
            Self::Cow(data) => Ok(data.clone()),
            Self::Py(py_str) => Ok(Cow::Borrowed(py_string_str(py_str)?)),
        }
    }

    pub fn as_py_string(&'a self, py: Python<'a>) -> &'a PyString {
        match self {
            Self::Cow(cow) => PyString::new(py, cow),
            Self::Py(py_string) => py_string,
        }
    }
}

impl<'a> From<&'a str> for EitherString<'a> {
    fn from(data: &'a str) -> Self {
        Self::Cow(Cow::Borrowed(data))
    }
}

impl<'a> From<&'a PyString> for EitherString<'a> {
    fn from(date: &'a PyString) -> Self {
        Self::Py(date)
    }
}

impl<'a> IntoPy<PyObject> for EitherString<'a> {
    fn into_py(self, py: Python<'_>) -> PyObject {
        self.as_py_string(py).into_py(py)
    }
}

pub fn py_string_str(py_str: &PyString) -> ValResult<&str> {
    py_str
        .to_str()
        .map_err(|_| ValError::new_custom_input(ErrorType::StringUnicode, InputValue::PyAny(py_str as &PyAny)))
}

#[cfg_attr(debug_assertions, derive(Debug))]
pub enum EitherBytes<'a> {
    Cow(Cow<'a, [u8]>),
    Py(&'a PyBytes),
}

impl<'a> From<Vec<u8>> for EitherBytes<'a> {
    fn from(date: Vec<u8>) -> Self {
        Self::Cow(Cow::Owned(date))
    }
}

impl<'a> From<&'a [u8]> for EitherBytes<'a> {
    fn from(date: &'a [u8]) -> Self {
        Self::Cow(Cow::Borrowed(date))
    }
}

impl<'a> From<&'a PyBytes> for EitherBytes<'a> {
    fn from(date: &'a PyBytes) -> Self {
        Self::Py(date)
    }
}

impl<'a> EitherBytes<'a> {
    pub fn len(&'a self) -> PyResult<usize> {
        match self {
            EitherBytes::Cow(bytes) => Ok(bytes.len()),
            EitherBytes::Py(py_bytes) => py_bytes.len(),
        }
    }
}

impl<'a> IntoPy<PyObject> for EitherBytes<'a> {
    fn into_py(self, py: Python<'_>) -> PyObject {
        match self {
            EitherBytes::Cow(bytes) => PyBytes::new(py, &bytes).into_py(py),
            EitherBytes::Py(py_bytes) => py_bytes.into_py(py),
        }
    }
}
