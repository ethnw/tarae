title: PHP: strlen - Manual
read: article
charset: UTF-8
links: 7
anchors: 7
---
# strlen

(PHP 4, PHP 5, PHP 7, PHP 8)

strlen — Get string length

### Description

function **strlen**([string](https://www.php.net/manual/en/language.types.string.php) `$string`): [int](https://www.php.net/manual/en/language.types.integer.php)

Returns the length of the given `string`.

### Parameters

`string`
  The [string](https://www.php.net/manual/en/language.types.string.php) being measured for length.

### Return Values

The length of the `string` in bytes.

### Examples

**Example #1 A strlen() example**

```
<?php
$str = 'abcdef';
echo strlen($str), PHP_EOL; // 6

$str = ' ab cd ';
echo strlen($str), PHP_EOL; // 7
?>
```

### Notes

> **Note**:
>
> **strlen()** returns the number of bytes rather than the number of characters in a string.

### See Also

- [count()](https://www.php.net/manual/en/function.count.php) - Counts all elements in an array or in a Countable object
- [grapheme_strlen()](https://www.php.net/manual/en/function.grapheme-strlen.php) - Get string length in grapheme units
- [iconv_strlen()](https://www.php.net/manual/en/function.iconv-strlen.php) - Returns the character count of string
- [mb_strlen()](https://www.php.net/manual/en/function.mb-strlen.php) - Get string length
